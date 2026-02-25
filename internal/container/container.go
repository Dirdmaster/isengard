// Package container provides Docker container inspection, listing, and recreation.
package container

import (
	"context"
	"fmt"
	"log/slog"

	"github.com/docker/docker/api/types"
	containertypes "github.com/docker/docker/api/types/container"
	"github.com/docker/docker/api/types/mount"
	"github.com/docker/docker/api/types/network"
	"github.com/docker/docker/client"
)

// Info captures the subset of container state needed for update checks
// and faithful recreation.
type Info struct {
	ID      string            // Docker container ID.
	Name    string            // Container name without leading slash.
	Image   string            // Image reference as specified at creation (e.g. "nginx:1.25").
	ImageID string            // Resolved image ID (sha256:...).
	Labels  map[string]string // Container labels, used for isengard.enable filtering.
	State   string            // Docker state string (running, exited, etc.).
	// RepoDigests from the image inspect, e.g. ["nginx@sha256:abc..."].
	// Used for fast digest comparison against the remote registry.
	RepoDigests []string
}

// ListRunning returns all running containers, enriching each with
// RepoDigests from an image inspect call for digest-based update checks.
func ListRunning(ctx context.Context, cli *client.Client) ([]Info, error) {
	containers, err := cli.ContainerList(ctx, containertypes.ListOptions{All: false})
	if err != nil {
		return nil, fmt.Errorf("listing containers: %w", err)
	}

	var result []Info
	for _, c := range containers {
		name := ""
		if len(c.Names) > 0 {
			name = c.Names[0]
			if len(name) > 0 && name[0] == '/' {
				name = name[1:]
			}
		}

		var repoDigests []string
		imgInspect, _, err := cli.ImageInspectWithRaw(ctx, c.ImageID)
		if err == nil {
			repoDigests = imgInspect.RepoDigests
		}

		result = append(result, Info{
			ID:          c.ID,
			Name:        name,
			Image:       c.Image,
			ImageID:     c.ImageID,
			Labels:      c.Labels,
			State:       c.State,
			RepoDigests: repoDigests,
		})
	}

	return result, nil
}

// Recreate stops, removes, and recreates a container with the same config
// but a new image. Returns the new container ID.
func Recreate(ctx context.Context, cli *client.Client, containerID string, newImageID string, stopTimeout int) (string, error) {
	inspect, err := cli.ContainerInspect(ctx, containerID)
	if err != nil {
		return "", fmt.Errorf("inspecting container: %w", err)
	}

	containerName := inspect.Name
	if len(containerName) > 0 && containerName[0] == '/' {
		containerName = containerName[1:]
	}

	slog.Debug("captured container config",
		"container", containerName,
		"old_image", inspect.Config.Image,
	)

	// Stop
	timeout := stopTimeout
	stopOpts := containertypes.StopOptions{Timeout: &timeout}
	if err := cli.ContainerStop(ctx, containerID, stopOpts); err != nil {
		slog.Warn("error stopping container, forcing remove", "container", containerName, "error", err)
	}

	// Remove
	if err := cli.ContainerRemove(ctx, containerID, containertypes.RemoveOptions{Force: true}); err != nil {
		return "", fmt.Errorf("removing container %s: %w", containerName, err)
	}

	// Build new config
	config := inspect.Config
	config.Image = newImageID

	hostConfig := inspect.HostConfig

	// Convert mounts back to proper mount configuration
	if len(inspect.Mounts) > 0 && len(hostConfig.Mounts) == 0 {
		hostConfig.Mounts = convertMounts(inspect.Mounts)
	}

	// Prepare networking — connect to the first network during create
	var networkingConfig *network.NetworkingConfig
	additionalNetworks := map[string]*network.EndpointSettings{}

	if inspect.NetworkSettings != nil && len(inspect.NetworkSettings.Networks) > 0 {
		first := true
		for netName, netSettings := range inspect.NetworkSettings.Networks {
			epSettings := &network.EndpointSettings{
				Aliases: netSettings.Aliases,
			}
			if netSettings.IPAMConfig != nil {
				epSettings.IPAMConfig = &network.EndpointIPAMConfig{
					IPv4Address: netSettings.IPAMConfig.IPv4Address,
				}
			}
			if first {
				networkingConfig = &network.NetworkingConfig{
					EndpointsConfig: map[string]*network.EndpointSettings{
						netName: epSettings,
					},
				}
				first = false
			} else {
				additionalNetworks[netName] = epSettings
			}
		}
	}

	// Create
	createResp, err := cli.ContainerCreate(ctx, config, hostConfig, networkingConfig, nil, containerName)
	if err != nil {
		return "", fmt.Errorf("creating container %s: %w", containerName, err)
	}

	// Connect additional networks
	for netName, epSettings := range additionalNetworks {
		if err := cli.NetworkConnect(ctx, netName, createResp.ID, epSettings); err != nil {
			slog.Warn("failed to connect network", "container", containerName, "network", netName, "error", err)
		}
	}

	// Start
	if err := cli.ContainerStart(ctx, createResp.ID, containertypes.StartOptions{}); err != nil {
		return "", fmt.Errorf("starting container %s: %w", containerName, err)
	}

	return createResp.ID, nil
}

// convertMounts converts docker inspect mount points back to mount.Mount format.
func convertMounts(mountPoints []types.MountPoint) []mount.Mount {
	var mounts []mount.Mount
	for _, mp := range mountPoints {
		m := mount.Mount{
			Type:     mount.Type(mp.Type),
			Source:   mp.Source,
			Target:   mp.Destination,
			ReadOnly: !mp.RW,
		}
		mounts = append(mounts, m)
	}
	return mounts
}
