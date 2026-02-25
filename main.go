package main

import (
	"context"
	"log/slog"
	"os"
	"os/signal"
	"syscall"
	"time"
)

func main() {
	cfg := LoadConfig()

	// Setup structured logging
	handler := slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{
		Level: cfg.LogLevel,
	})
	slog.SetDefault(slog.New(handler))

	slog.Info("starting isengard",
		"interval", cfg.Interval,
		"run_once", cfg.RunOnce,
		"cleanup", cfg.Cleanup,
		"stop_timeout", cfg.StopTimeout,
	)

	// Create Docker client
	cli, err := NewDockerClient()
	if err != nil {
		slog.Error("failed to create Docker client", "error", err)
		os.Exit(1)
	}
	defer cli.Close()

	// Verify Docker connectivity
	info, err := cli.Info(context.Background())
	if err != nil {
		slog.Error("failed to connect to Docker", "error", err)
		os.Exit(1)
	}
	slog.Info("connected to Docker",
		"version", info.ServerVersion,
		"containers", info.Containers,
	)

	updater := NewUpdater(cli, cfg)

	// Setup graceful shutdown
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)

	go func() {
		sig := <-sigCh
		slog.Info("received signal, shutting down gracefully", "signal", sig)
		cancel()
	}()

	// Run the first cycle immediately
	runCycle(ctx, updater)

	if cfg.RunOnce {
		slog.Info("run-once mode, exiting")
		return
	}

	// Schedule subsequent cycles
	ticker := time.NewTicker(cfg.Interval)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			slog.Info("shutting down")
			return
		case <-ticker.C:
			runCycle(ctx, updater)
		}
	}
}

// runCycle executes a single update cycle with error handling.
func runCycle(ctx context.Context, updater *Updater) {
	if ctx.Err() != nil {
		return
	}

	updated, err := updater.RunCycle(ctx)
	if err != nil {
		slog.Error("update cycle failed", "error", err)
		return
	}

	if updated > 0 {
		slog.Info("cycle finished", "updated", updated)
	}
}
