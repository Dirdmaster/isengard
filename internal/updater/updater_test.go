package updater

import (
	"os"
	"path/filepath"
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

func TestIsHex(t *testing.T) {
	tests := []struct {
		input    string
		expected bool
	}{
		{"abc123def456", true},
		{"0123456789abcdef", true},
		{"ABCDEF", false},       // uppercase not allowed
		{"abc123def45g", false}, // 'g' is not hex
		{"isengard", false},     // service name from compose
		{"my-container", false}, // contains hyphen
		{"", false},             // empty string
		{"0bb7fda0c98c", true},  // real short container ID
	}

	for _, tt := range tests {
		t.Run(tt.input, func(t *testing.T) {
			got := isHex(tt.input)
			if got != tt.expected {
				t.Errorf("isHex(%q): got %v, want %v", tt.input, got, tt.expected)
			}
		})
	}
}

func TestParseMountinfo(t *testing.T) {
	expectedID := "0bb7fda0c98c1f9200b374f681f1bb7a08b60dc56dc3c22fc25cfbcf42b7720b"

	tests := []struct {
		name     string
		content  string
		expected string
	}{
		{
			name: "cgroup v2 with docker overlay mounts",
			content: `29 1 0:27 / / rw,relatime - overlay overlay rw,lowerdir=/var/lib/docker/overlay2/l/ABC
653 29 0:30 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw
723 29 254:1 /var/lib/docker/containers/0bb7fda0c98c1f9200b374f681f1bb7a08b60dc56dc3c22fc25cfbcf42b7720b/resolv.conf /etc/resolv.conf rw,relatime - ext4 /dev/vda1 rw
724 29 254:1 /var/lib/docker/containers/0bb7fda0c98c1f9200b374f681f1bb7a08b60dc56dc3c22fc25cfbcf42b7720b/hostname /etc/hostname rw,relatime - ext4 /dev/vda1 rw
725 29 254:1 /var/lib/docker/containers/0bb7fda0c98c1f9200b374f681f1bb7a08b60dc56dc3c22fc25cfbcf42b7720b/hosts /etc/hosts rw,relatime - ext4 /dev/vda1 rw
`,
			expected: expectedID,
		},
		{
			name: "cgroup v1 with docker overlay mounts",
			content: `22 1 0:20 / / rw,relatime - overlay overlay rw,lowerdir=/var/lib/docker/overlay2/l/XYZ
100 22 0:25 /docker/containers/0bb7fda0c98c1f9200b374f681f1bb7a08b60dc56dc3c22fc25cfbcf42b7720b/hostname /etc/hostname rw - ext4 /dev/sda1 rw
`,
			expected: expectedID,
		},
		{
			name:     "not in a container",
			content:  `29 1 0:27 / / rw,relatime - ext4 /dev/sda1 rw`,
			expected: "",
		},
		{
			name:     "empty file",
			content:  "",
			expected: "",
		},
		{
			name: "podman style path",
			content: `723 29 254:1 /var/lib/containers/storage/overlay-containers/0bb7fda0c98c1f9200b374f681f1bb7a08b60dc56dc3c22fc25cfbcf42b7720b/userdata/hostname /etc/hostname rw - ext4 /dev/sda1 rw
`,
			expected: "",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			// Write mountinfo content to a temp file
			dir := t.TempDir()
			path := filepath.Join(dir, "mountinfo")
			if err := os.WriteFile(path, []byte(tt.content), 0o644); err != nil {
				t.Fatal(err)
			}

			got := parseMountinfo(path)
			if got != tt.expected {
				t.Errorf("parseMountinfo(): got %q, want %q", got, tt.expected)
			}
		})
	}
}

func TestParseMountinfoNonexistentFile(t *testing.T) {
	got := parseMountinfo("/nonexistent/path/mountinfo")
	if got != "" {
		t.Errorf("expected empty string for nonexistent file, got %q", got)
	}
}
