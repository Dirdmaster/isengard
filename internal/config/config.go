// Package config handles Isengard configuration from environment variables.
package config

import (
	"log/slog"
	"os"
	"strconv"
	"time"
)

// Config holds all Isengard configuration parsed from environment variables.
type Config struct {
	Interval    time.Duration
	RunOnce     bool
	Cleanup     bool
	WatchAll    bool
	StopTimeout int
	LogLevel    slog.Level
}

// Load reads configuration from environment variables with sensible defaults.
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
