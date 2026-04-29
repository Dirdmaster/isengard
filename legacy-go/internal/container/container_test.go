package container

import (
	"testing"

	containertypes "github.com/docker/docker/api/types/container"
	"github.com/docker/docker/api/types/mount"
)

func TestDeduplicateVolumes_OverlapWithMounts(t *testing.T) {
	config := &containertypes.Config{
		Volumes: map[string]struct{}{
			"/config": {},
			"/data":   {},
		},
	}
	hostConfig := &containertypes.HostConfig{
		Mounts: []mount.Mount{
			{Type: mount.TypeBind, Source: "/host/config", Target: "/config"},
		},
	}

	deduplicateVolumes(config, hostConfig)

	if _, ok := config.Volumes["/config"]; ok {
		t.Error("/config should have been removed from Volumes (covered by Mounts)")
	}
	if _, ok := config.Volumes["/data"]; !ok {
		t.Error("/data should still be in Volumes (not covered by Mounts)")
	}
}

func TestDeduplicateVolumes_OverlapWithBinds(t *testing.T) {
	config := &containertypes.Config{
		Volumes: map[string]struct{}{
			"/config": {},
		},
	}
	hostConfig := &containertypes.HostConfig{
		Binds: []string{"/host/config:/config:rw"},
	}

	deduplicateVolumes(config, hostConfig)

	if _, ok := config.Volumes["/config"]; ok {
		t.Error("/config should have been removed from Volumes (covered by Binds)")
	}
}

func TestDeduplicateVolumes_AllOverlap(t *testing.T) {
	config := &containertypes.Config{
		Volumes: map[string]struct{}{
			"/config": {},
			"/backup": {},
		},
	}
	hostConfig := &containertypes.HostConfig{
		Mounts: []mount.Mount{
			{Type: mount.TypeVolume, Source: "config-vol", Target: "/config"},
			{Type: mount.TypeBind, Source: "/host/backup", Target: "/backup"},
		},
	}

	deduplicateVolumes(config, hostConfig)

	if len(config.Volumes) != 0 {
		t.Errorf("expected empty Volumes map, got %v", config.Volumes)
	}
}

func TestDeduplicateVolumes_NoOverlap(t *testing.T) {
	config := &containertypes.Config{
		Volumes: map[string]struct{}{
			"/data": {},
		},
	}
	hostConfig := &containertypes.HostConfig{
		Mounts: []mount.Mount{
			{Type: mount.TypeBind, Source: "/host/config", Target: "/config"},
		},
	}

	deduplicateVolumes(config, hostConfig)

	if _, ok := config.Volumes["/data"]; !ok {
		t.Error("/data should still be in Volumes (no overlap)")
	}
}

func TestDeduplicateVolumes_NilVolumes(t *testing.T) {
	config := &containertypes.Config{
		Volumes: nil,
	}
	hostConfig := &containertypes.HostConfig{
		Mounts: []mount.Mount{
			{Type: mount.TypeBind, Source: "/host/config", Target: "/config"},
		},
	}

	// Should not panic
	deduplicateVolumes(config, hostConfig)

	if config.Volumes != nil {
		t.Errorf("expected nil Volumes, got %v", config.Volumes)
	}
}

func TestDeduplicateVolumes_NoMountsOrBinds(t *testing.T) {
	config := &containertypes.Config{
		Volumes: map[string]struct{}{
			"/data": {},
		},
	}
	hostConfig := &containertypes.HostConfig{}

	deduplicateVolumes(config, hostConfig)

	if _, ok := config.Volumes["/data"]; !ok {
		t.Error("/data should still be in Volumes (nothing to deduplicate against)")
	}
}

func TestDeduplicateVolumes_BindsWithoutOptions(t *testing.T) {
	config := &containertypes.Config{
		Volumes: map[string]struct{}{
			"/data": {},
		},
	}
	hostConfig := &containertypes.HostConfig{
		Binds: []string{"/host/data:/data"},
	}

	deduplicateVolumes(config, hostConfig)

	if _, ok := config.Volumes["/data"]; ok {
		t.Error("/data should have been removed from Volumes (covered by Binds without options suffix)")
	}
}

func TestConvertMounts_BindRW(t *testing.T) {
	input := []containertypes.MountPoint{
		{Type: mount.TypeBind, Source: "/host/path", Destination: "/container/path", RW: true},
	}

	result := convertMounts(input)

	if len(result) != 1 {
		t.Fatalf("expected 1 mount, got %d", len(result))
	}
	m := result[0]
	if m.Type != mount.TypeBind {
		t.Errorf("expected type bind, got %s", m.Type)
	}
	if m.Source != "/host/path" {
		t.Errorf("expected source /host/path, got %s", m.Source)
	}
	if m.Target != "/container/path" {
		t.Errorf("expected target /container/path, got %s", m.Target)
	}
	if m.ReadOnly {
		t.Error("expected ReadOnly false for RW mount")
	}
}

func TestConvertMounts_VolumeRO(t *testing.T) {
	input := []containertypes.MountPoint{
		{Type: mount.TypeVolume, Source: "my-vol", Destination: "/data", RW: false},
	}

	result := convertMounts(input)

	if len(result) != 1 {
		t.Fatalf("expected 1 mount, got %d", len(result))
	}
	m := result[0]
	if m.Type != mount.TypeVolume {
		t.Errorf("expected type volume, got %s", m.Type)
	}
	if m.Source != "my-vol" {
		t.Errorf("expected source my-vol, got %s", m.Source)
	}
	if m.Target != "/data" {
		t.Errorf("expected target /data, got %s", m.Target)
	}
	if !m.ReadOnly {
		t.Error("expected ReadOnly true for RO mount")
	}
}

func TestConvertMounts_Empty(t *testing.T) {
	result := convertMounts(nil)
	if len(result) != 0 {
		t.Errorf("expected empty result, got %d mounts", len(result))
	}
}
