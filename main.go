package main

import (
	"context"
	"fmt"
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
	if err := run(); err != nil {
		slog.Error("fatal", "error", err)
		os.Exit(1)
	}
}

func run() error {
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
		return fmt.Errorf("creating Docker client: %w", err)
	}
	defer cli.Close()

	info, err := cli.Info(context.Background())
	if err != nil {
		return fmt.Errorf("connecting to Docker: %w", err)
	}
	slog.Info("connected to Docker",
		"version", info.ServerVersion,
		"containers", info.Containers,
	)

	checkDockerConfig()

	u := updater.New(cli, cfg)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)

	go func() {
		sig := <-sigCh
		slog.Info("received signal, shutting down gracefully", "signal", sig)
		cancel()
	}()

	runCycle(ctx, u)

	if cfg.RunOnce {
		slog.Info("run-once mode, exiting")
		return nil
	}

	ticker := time.NewTicker(cfg.Interval)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			slog.Info("shutting down")
			return nil
		case <-ticker.C:
			runCycle(ctx, u)
		}
	}
}

// checkDockerConfig warns at startup if the Docker config path exists but is
// a directory. This typically means Docker created it automatically when the
// file was bind-mounted but did not exist on the host.
func checkDockerConfig() {
	configPath := "/root/.docker/config.json"
	if v := os.Getenv("DOCKER_CONFIG"); v != "" {
		configPath = v + "/config.json"
	}

	fi, err := os.Stat(configPath)
	if err != nil {
		return // does not exist, nothing to warn about
	}
	if fi.IsDir() {
		slog.Warn("docker config path is a directory, not a file (private registry auth will not work)",
			"path", configPath,
			"hint", "remove the directory on the host and only mount config.json if the file exists",
		)
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
