package updater

import (
	"testing"

	"github.com/dirdmaster/isengard/internal/config"
	"github.com/dirdmaster/isengard/internal/container"
)

func TestIsSelf(t *testing.T) {
	tests := []struct {
		name        string
		selfID      string
		containerID string
		expected    bool
	}{
		{
			name:        "exact match full ID",
			selfID:      "abc123def456abc123def456abc123def456abc123def456abc123def456abcd",
			containerID: "abc123def456abc123def456abc123def456abc123def456abc123def456abcd",
			expected:    true,
		},
		{
			name:        "short ID matches full ID prefix",
			selfID:      "abc123def456",
			containerID: "abc123def456abc123def456abc123def456abc123def456abc123def456abcd",
			expected:    true,
		},
		{
			name:        "different container",
			selfID:      "abc123def456",
			containerID: "zzz999aaa888zzz999aaa888zzz999aaa888zzz999aaa888zzz999aaa888zzzz",
			expected:    false,
		},
		{
			name:        "empty selfID means not in container",
			selfID:      "",
			containerID: "abc123def456abc123def456abc123def456abc123def456abc123def456abcd",
			expected:    false,
		},
		{
			name:        "partial match but not prefix",
			selfID:      "def456abc123",
			containerID: "abc123def456abc123def456abc123def456abc123def456abc123def456abcd",
			expected:    false,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			u := &Updater{selfID: tt.selfID}
			got := u.isSelf(tt.containerID)
			if got != tt.expected {
				t.Errorf("isSelf(%q) with selfID=%q: got %v, want %v",
					tt.containerID, tt.selfID, got, tt.expected)
			}
		})
	}
}

func TestShouldSkip(t *testing.T) {
	selfID := "aabbccddee11"
	selfFullID := "aabbccddee11aabbccddee11aabbccddee11aabbccddee11aabbccddee11aabb"
	otherID := "ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00"

	tests := []struct {
		name     string
		watchAll bool
		c        container.Info
		expected bool
	}{
		{
			name:     "skip self by short ID prefix",
			watchAll: true,
			c:        container.Info{ID: selfFullID, Name: "isengard", Image: "ghcr.io/dirdmaster/isengard:latest"},
			expected: true,
		},
		{
			name:     "watch-all: include unlabeled container",
			watchAll: true,
			c:        container.Info{ID: otherID, Name: "nginx", Image: "nginx:latest", Labels: map[string]string{}},
			expected: false,
		},
		{
			name:     "watch-all: skip explicitly disabled",
			watchAll: true,
			c:        container.Info{ID: otherID, Name: "nginx", Image: "nginx:latest", Labels: map[string]string{"isengard.enable": "false"}},
			expected: true,
		},
		{
			name:     "opt-in: skip unlabeled container",
			watchAll: false,
			c:        container.Info{ID: otherID, Name: "nginx", Image: "nginx:latest", Labels: map[string]string{}},
			expected: true,
		},
		{
			name:     "opt-in: include labeled container",
			watchAll: false,
			c:        container.Info{ID: otherID, Name: "nginx", Image: "nginx:latest", Labels: map[string]string{"isengard.enable": "true"}},
			expected: false,
		},
		{
			name:     "skip sha256 image ref",
			watchAll: true,
			c:        container.Info{ID: otherID, Name: "custom", Image: "sha256:abcdef", Labels: map[string]string{}},
			expected: true,
		},
		{
			name:     "skip empty image ref",
			watchAll: true,
			c:        container.Info{ID: otherID, Name: "custom", Image: "", Labels: map[string]string{}},
			expected: true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			u := &Updater{
				selfID: selfID,
				config: config.Config{WatchAll: tt.watchAll},
			}
			got := u.shouldSkip(tt.c)
			if got != tt.expected {
				t.Errorf("shouldSkip(%q): got %v, want %v", tt.c.Name, got, tt.expected)
			}
		})
	}
}

func TestShouldSkipLabelCaseInsensitive(t *testing.T) {
	u := &Updater{
		selfID: "",
		config: config.Config{WatchAll: true},
	}

	c := container.Info{
		ID:     "ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00",
		Name:   "nginx",
		Image:  "nginx:latest",
		Labels: map[string]string{"isengard.enable": "False"},
	}

	if !u.shouldSkip(c) {
		t.Error("expected shouldSkip=true for isengard.enable=False (case insensitive)")
	}
}
