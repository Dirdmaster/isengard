package config

import (
	"log/slog"
	"os"
	"testing"
	"time"
)

func TestLoadDefaults(t *testing.T) {
	// Clear all ISENGARD_ env vars
	for _, key := range []string{
		"ISENGARD_INTERVAL", "ISENGARD_RUN_ONCE", "ISENGARD_CLEANUP",
		"ISENGARD_WATCH_ALL", "ISENGARD_STOP_TIMEOUT", "ISENGARD_LOG_LEVEL",
		"ISENGARD_SELF_UPDATE",
	} {
		os.Unsetenv(key)
	}

	cfg := Load()

	if cfg.Interval != 30*time.Minute {
		t.Errorf("expected interval 30m, got %v", cfg.Interval)
	}
	if cfg.RunOnce {
		t.Error("expected RunOnce false")
	}
	if !cfg.Cleanup {
		t.Error("expected Cleanup true")
	}
	if !cfg.WatchAll {
		t.Error("expected WatchAll true")
	}
	if cfg.StopTimeout != 30 {
		t.Errorf("expected StopTimeout 30, got %d", cfg.StopTimeout)
	}
	if cfg.LogLevel != slog.LevelInfo {
		t.Errorf("expected LogLevel info, got %v", cfg.LogLevel)
	}
	if cfg.SelfUpdate {
		t.Error("expected SelfUpdate false")
	}
}

func TestLoadSelfUpdate(t *testing.T) {
	tests := []struct {
		name     string
		envVal   string
		expected bool
	}{
		{"true", "true", true},
		{"false", "false", false},
		{"1", "1", true},
		{"0", "0", false},
		{"invalid", "yes", false}, // ParseBool fails, keeps default
		{"empty", "", false},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if tt.envVal == "" {
				os.Unsetenv("ISENGARD_SELF_UPDATE")
			} else {
				os.Setenv("ISENGARD_SELF_UPDATE", tt.envVal)
			}
			defer os.Unsetenv("ISENGARD_SELF_UPDATE")

			cfg := Load()
			if cfg.SelfUpdate != tt.expected {
				t.Errorf("ISENGARD_SELF_UPDATE=%q: expected SelfUpdate=%v, got %v",
					tt.envVal, tt.expected, cfg.SelfUpdate)
			}
		})
	}
}

func TestLoadInterval(t *testing.T) {
	os.Setenv("ISENGARD_INTERVAL", "5m")
	defer os.Unsetenv("ISENGARD_INTERVAL")

	cfg := Load()
	if cfg.Interval != 5*time.Minute {
		t.Errorf("expected interval 5m, got %v", cfg.Interval)
	}
}

func TestLoadInvalidInterval(t *testing.T) {
	os.Setenv("ISENGARD_INTERVAL", "-1s")
	defer os.Unsetenv("ISENGARD_INTERVAL")

	cfg := Load()
	if cfg.Interval != 30*time.Minute {
		t.Errorf("expected default interval 30m for invalid input, got %v", cfg.Interval)
	}
}

func TestLoadLogLevel(t *testing.T) {
	tests := []struct {
		envVal   string
		expected slog.Level
	}{
		{"debug", slog.LevelDebug},
		{"warn", slog.LevelWarn},
		{"error", slog.LevelError},
		{"info", slog.LevelInfo},
		{"unknown", slog.LevelInfo},
	}

	for _, tt := range tests {
		t.Run(tt.envVal, func(t *testing.T) {
			os.Setenv("ISENGARD_LOG_LEVEL", tt.envVal)
			defer os.Unsetenv("ISENGARD_LOG_LEVEL")

			cfg := Load()
			if cfg.LogLevel != tt.expected {
				t.Errorf("ISENGARD_LOG_LEVEL=%q: expected %v, got %v",
					tt.envVal, tt.expected, cfg.LogLevel)
			}
		})
	}
}
