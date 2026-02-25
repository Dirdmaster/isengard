package main

import (
	"context"
	"log/slog"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/charmbracelet/log"
	"github.com/muesli/termenv"

	"github.com/dirdmaster/isengard/internal/config"
	"github.com/dirdmaster/isengard/internal/docker"
	"github.com/dirdmaster/isengard/internal/updater"
)

func main() {
	cfg := config.Load()

	// Setup pretty logging via charmbracelet/log
	logger := log.NewWithOptions(os.Stdout, log.Options{
		Level:           log.Level(cfg.LogLevel),
		ReportTimestamp: true,
	})
	// Force color output — Docker containers have no TTY so charm
	// disables colors by default. This sets it on the logger's own renderer.
	logger.SetColorProfile(termenv.TrueColor)
	slog.SetDefault(slog.New(logger))

	slog.Info("starting isengard",
		"interval", cfg.Interval,
		"run_once", cfg.RunOnce,
		"cleanup", cfg.Cleanup,
		"stop_timeout", cfg.StopTimeout,
	)

	cli, err := docker.NewClient()
	if err != nil {
		slog.Error("failed to create Docker client", "error", err)
		os.Exit(1)
	}
	defer cli.Close()

	info, err := cli.Info(context.Background())
	if err != nil {
		slog.Error("failed to connect to Docker", "error", err)
		os.Exit(1)
	}
	slog.Info("connected to Docker",
		"version", info.ServerVersion,
		"containers", info.Containers,
	)

	u := updater.New(cli, cfg)

	// Graceful shutdown
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)

	go func() {
		sig := <-sigCh
		slog.Info("received signal, shutting down gracefully", "signal", sig)
		cancel()
	}()

	// Run first cycle immediately
	runCycle(ctx, u)

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
			runCycle(ctx, u)
		}
	}
}

func runCycle(ctx context.Context, u *updater.Updater) {
	if ctx.Err() != nil {
		return
	}

	updated, err := u.RunCycle(ctx)
	if err != nil {
		slog.Error("update cycle failed", "error", err)
		return
	}

	if updated > 0 {
		slog.Info("cycle finished", "updated", updated)
	}
}
