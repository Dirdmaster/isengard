// Package config handles Isengard configuration from environment variables.
package config

import (
	"log/slog"
	"os"
	"strconv"
	"time"
)

// Config controls Isengard's runtime behavior.
// All fields map to ISENGARD_* environment variables via [Load].
type Config struct {
	// Interval between update check cycles (ISENGARD_INTERVAL, default 30m).
	Interval time.Duration
	// RunOnce exits after a single check cycle (ISENGARD_RUN_ONCE).
	RunOnce bool
	// Cleanup removes old images after a successful update (ISENGARD_CLEANUP, default true).
	Cleanup bool
	// WatchAll watches every container by default; when false, only containers
	// labeled isengard.enable=true are watched (ISENGARD_WATCH_ALL, default true).
	WatchAll bool
	// StopTimeout is the grace period in seconds before force-killing a container (ISENGARD_STOP_TIMEOUT, default 30).
	StopTimeout int
	// LogLevel sets the minimum log severity (ISENGARD_LOG_LEVEL: debug, info, warn, error).
	LogLevel slog.Level
}

// Load populates a [Config] from ISENGARD_* environment variables,
// falling back to defaults for any variable that is unset or invalid.
func Load() Config {
	c := Config{
		Interval:    30 * time.Minute,
		RunOnce:     false,
		Cleanup:     true,
		WatchAll:    true,
		StopTimeout: 30,
		LogLevel:    slog.LevelInfo,
	}

	if v := os.Getenv("ISENGARD_INTERVAL"); v != "" {
		d, err := time.ParseDuration(v)
		if err == nil && d > 0 {
			c.Interval = d
		}
	}

	if v := os.Getenv("ISENGARD_RUN_ONCE"); v != "" {
		c.RunOnce, _ = strconv.ParseBool(v)
	}

	if v := os.Getenv("ISENGARD_CLEANUP"); v != "" {
		c.Cleanup, _ = strconv.ParseBool(v)
	}

	if v := os.Getenv("ISENGARD_WATCH_ALL"); v != "" {
		c.WatchAll, _ = strconv.ParseBool(v)
	}

	if v := os.Getenv("ISENGARD_STOP_TIMEOUT"); v != "" {
		n, err := strconv.Atoi(v)
		if err == nil && n > 0 {
			c.StopTimeout = n
		}
	}

	if v := os.Getenv("ISENGARD_LOG_LEVEL"); v != "" {
		switch v {
		case "debug":
			c.LogLevel = slog.LevelDebug
		case "warn":
			c.LogLevel = slog.LevelWarn
		case "error":
			c.LogLevel = slog.LevelError
		default:
			c.LogLevel = slog.LevelInfo
		}
	}

	return c
}
