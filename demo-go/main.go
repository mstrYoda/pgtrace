package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"os"
	"time"

	_ "github.com/jackc/pgx/v5/stdlib"
	"go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/exporters/otlp/otlptrace/otlptracehttp"
	"go.opentelemetry.io/otel/propagation"
	"go.opentelemetry.io/otel/sdk/resource"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	semconv "go.opentelemetry.io/otel/semconv/v1.20.0"
	"go.opentelemetry.io/otel/trace"
	"gorm.io/driver/postgres"
	"gorm.io/gorm"
	"gorm.io/gorm/logger"
)

var tracer = otel.Tracer("demo-go")

// User model
type User struct {
	ID        uint      `gorm:"primarykey"`
	Name      string    `gorm:"size:255"`
	Email     string    `gorm:"size:255;uniqueIndex"`
	CreatedAt time.Time `json:"created_at"`
	UpdatedAt time.Time `json:"updated_at"`
}

func main() {
	ctx := context.Background()

	// ── Init OpenTelemetry ──────────────────────────────
	tp, err := initTracer(ctx)
	if err != nil {
		log.Fatalf("init tracer: %v", err)
	}
	defer func() { _ = tp.Shutdown(ctx) }()

	// ── Connect to Postgres via GORM ────────────────────
	dsn := os.Getenv("DATABASE_URL")
	if dsn == "" {
		dsn = "host=postgres user=postgres password=secret dbname=postgres port=5432 sslmode=disable"
	}

	sqlDB, err := sql.Open("pgx", dsn)
	if err != nil {
		log.Fatalf("open sql: %v", err)
	}
	if err := sqlDB.Ping(); err != nil {
		log.Fatalf("ping db: %v", err)
	}

	// Wrap the raw *sql.DB so every query gets a traceparent comment
	tracingPool := &tracingConnPool{db: sqlDB}

	gormDB, err := gorm.Open(postgres.New(postgres.Config{
		Conn: tracingPool,
	}), &gorm.Config{
		Logger: logger.Default.LogMode(logger.Info),
	})
	if err != nil {
		log.Fatalf("open gorm: %v", err)
	}

	if err := gormDB.AutoMigrate(&User{}); err != nil {
		log.Fatalf("migrate: %v", err)
	}

	// ── HTTP handlers ───────────────────────────────────
	mux := http.NewServeMux()
	mux.HandleFunc("/health", handleHealth)
	mux.HandleFunc("/users", func(w http.ResponseWriter, r *http.Request) {
		switch r.Method {
		case http.MethodPost:
			handleCreateUser(w, r, gormDB)
		case http.MethodGet:
			handleListUsers(w, r, gormDB)
		default:
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		}
	})

	handler := otelhttp.NewHandler(mux, "demo-go-server",
		otelhttp.WithSpanNameFormatter(func(_ string, r *http.Request) string {
			return fmt.Sprintf("%s %s", r.Method, r.URL.Path)
		}),
	)

	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}
	log.Printf("Server running on :%s", port)
	log.Fatal(http.ListenAndServe(":"+port, handler))
}

// ── HTTP handlers ─────────────────────────────────────────

func handleHealth(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(map[string]bool{"ok": true})
}

func handleCreateUser(w http.ResponseWriter, r *http.Request, db *gorm.DB) {
	ctx := r.Context()
	var req struct {
		Name  string `json:"name"`
		Email string `json:"email"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}

	user := User{Name: req.Name, Email: req.Email}
	if err := db.WithContext(ctx).Create(&user).Error; err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusCreated)
	json.NewEncoder(w).Encode(user)
}

func handleListUsers(w http.ResponseWriter, r *http.Request, db *gorm.DB) {
	ctx := r.Context()
	var users []User
	if err := db.WithContext(ctx).Find(&users).Error; err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(users)
}

// ── OpenTelemetry setup ───────────────────────────────────

func initTracer(ctx context.Context) (*sdktrace.TracerProvider, error) {
	endpoint := os.Getenv("OTEL_EXPORTER_OTLP_ENDPOINT")
	if endpoint == "" {
		endpoint = "otel-collector:4318"
	}

	exporter, err := otlptracehttp.New(ctx,
		otlptracehttp.WithEndpoint(endpoint),
		otlptracehttp.WithURLPath("/v1/traces"),
		otlptracehttp.WithInsecure(),
	)
	if err != nil {
		return nil, err
	}

	tp := sdktrace.NewTracerProvider(
		sdktrace.WithBatcher(exporter),
		sdktrace.WithResource(resource.NewWithAttributes(
			semconv.SchemaURL,
			semconv.ServiceNameKey.String("demo-go"),
			semconv.ServiceVersionKey.String("0.1.0"),
		)),
	)
	otel.SetTracerProvider(tp)
	otel.SetTextMapPropagator(propagation.TraceContext{})
	return tp, nil
}

// ── tracingConnPool ───────────────────────────────────────
// Wraps *sql.DB to inject the current W3C traceparent as a SQL comment.

type tracingConnPool struct {
	db *sql.DB
}

func (p *tracingConnPool) ExecContext(ctx context.Context, query string, args ...interface{}) (sql.Result, error) {
	return p.db.ExecContext(ctx, injectTraceparent(ctx, query), args...)
}

func (p *tracingConnPool) QueryContext(ctx context.Context, query string, args ...interface{}) (*sql.Rows, error) {
	return p.db.QueryContext(ctx, injectTraceparent(ctx, query), args...)
}

func (p *tracingConnPool) QueryRowContext(ctx context.Context, query string, args ...interface{}) *sql.Row {
	return p.db.QueryRowContext(ctx, injectTraceparent(ctx, query), args...)
}

func (p *tracingConnPool) PrepareContext(ctx context.Context, query string) (*sql.Stmt, error) {
	return p.db.PrepareContext(ctx, injectTraceparent(ctx, query))
}

func (p *tracingConnPool) BeginTx(ctx context.Context, opts *sql.TxOptions) (*sql.Tx, error) {
	return p.db.BeginTx(ctx, opts)
}

// injectTraceparent appends a sqlcommenter-style traceparent comment.
func injectTraceparent(ctx context.Context, query string) string {
	span := trace.SpanFromContext(ctx)
	if !span.SpanContext().IsValid() {
		return query
	}
	sc := span.SpanContext()
	// W3C traceparent: 00-<trace-id>-<parent-id>-<flags>
	tp := fmt.Sprintf("00-%s-%s-%02x",
		sc.TraceID().String(),
		sc.SpanID().String(),
		sc.TraceFlags(),
	)
	return query + fmt.Sprintf(" /*traceparent='%s'*/", tp)
}
