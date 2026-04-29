// Package registry provides Docker registry v2 API interactions for digest
// checking and authentication. It supports Docker Hub, GHCR, ECR, Quay,
// and self-hosted registries.
package registry

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"log/slog"
	"net/http"
	"os"
	"strings"
)

// ImageRef is a parsed Docker image reference.
type ImageRef struct {
	// Registry is the hostname (e.g. "registry-1.docker.io", "ghcr.io").
	Registry string
	// Repository is the full repository path (e.g. "library/nginx", "user/repo").
	Repository string
	// Tag is the image tag (defaults to "latest").
	Tag string
}

// ParseImageRef parses a Docker image reference string into its components.
//
//	"nginx"                         -> registry-1.docker.io / library/nginx : latest
//	"nginx:1.25"                    -> registry-1.docker.io / library/nginx : 1.25
//	"user/repo:tag"                 -> registry-1.docker.io / user/repo    : tag
//	"ghcr.io/user/repo:v1"         -> ghcr.io             / user/repo    : v1
//	"registry.example.com:5000/img" -> registry.example.com:5000 / img    : latest
func ParseImageRef(imageRef string) ImageRef {
	ref := imageRef

	// Remove digest if present (we're resolving by tag)
	if i := strings.LastIndex(ref, "@"); i >= 0 {
		ref = ref[:i]
	}

	// Extract tag
	tag := "latest"
	// Find last colon — but only if it's after the last slash (colons before slash are ports)
	if lastSlash := strings.LastIndex(ref, "/"); lastSlash >= 0 {
		if lastColon := strings.LastIndex(ref, ":"); lastColon > lastSlash {
			tag = ref[lastColon+1:]
			ref = ref[:lastColon]
		}
	} else {
		// No slash at all — simple image name, colon is always a tag
		if lastColon := strings.LastIndex(ref, ":"); lastColon >= 0 {
			tag = ref[lastColon+1:]
			ref = ref[:lastColon]
		}
	}

	// Determine registry and repository
	registry := "registry-1.docker.io"
	repository := ref

	if strings.Contains(ref, "/") {
		parts := strings.SplitN(ref, "/", 2)
		first := parts[0]

		// Looks like a registry hostname if it has dots, colons, or is "localhost"
		if strings.Contains(first, ".") || strings.Contains(first, ":") || first == "localhost" {
			registry = first
			repository = parts[1]

			// Docker Hub aliases
			if registry == "docker.io" || registry == "index.docker.io" {
				registry = "registry-1.docker.io"
			}
		}
	}

	// Docker Hub library images: "nginx" -> "library/nginx"
	if registry == "registry-1.docker.io" && !strings.Contains(repository, "/") {
		repository = "library/" + repository
	}

	return ImageRef{
		Registry:   registry,
		Repository: repository,
		Tag:        tag,
	}
}

// RegistryURL returns the v2 API base URL for this registry.
func (r ImageRef) RegistryURL() string {
	return "https://" + r.Registry + "/v2"
}

// ManifestURL returns the full URL for the tag's manifest.
func (r ImageRef) ManifestURL() string {
	return fmt.Sprintf("%s/%s/manifests/%s", r.RegistryURL(), r.Repository, r.Tag)
}

// CheckDigest queries the registry v2 API to get the remote manifest digest
// for the given image reference. Returns the digest string (e.g. "sha256:abc...")
// or an error if the check fails.
//
// The flow:
//  1. HEAD request to the manifest endpoint
//  2. If 401, extract Www-Authenticate challenge, exchange for a Bearer token
//  3. Retry HEAD with Bearer token
//  4. Return the Docker-Content-Digest header value
func CheckDigest(imageRef string) (string, error) {
	ref := ParseImageRef(imageRef)
	manifestURL := ref.ManifestURL()

	slog.Debug("checking remote digest",
		"registry", ref.Registry,
		"repository", ref.Repository,
		"tag", ref.Tag,
		"url", manifestURL,
	)

	// Accept headers needed to get the correct digest for multi-arch manifests
	acceptHeaders := []string{
		"application/vnd.docker.distribution.manifest.v2+json",
		"application/vnd.docker.distribution.manifest.list.v2+json",
		"application/vnd.oci.image.manifest.v1+json",
		"application/vnd.oci.image.index.v1+json",
	}

	// First attempt: unauthenticated HEAD
	req, err := http.NewRequest("HEAD", manifestURL, http.NoBody)
	if err != nil {
		return "", fmt.Errorf("creating request: %w", err)
	}
	for _, accept := range acceptHeaders {
		req.Header.Add("Accept", accept)
	}

	// If we have Basic credentials for this registry, add them upfront
	if username, password, ok := credentialsForRegistry(ref.Registry); ok {
		req.SetBasicAuth(username, password)
	}

	client := &http.Client{}
	resp, err := client.Do(req)
	if err != nil {
		return "", fmt.Errorf("HEAD manifest: %w", err)
	}
	resp.Body.Close()

	// If we get a digest immediately, great
	if resp.StatusCode == http.StatusOK {
		digest := resp.Header.Get("Docker-Content-Digest")
		if digest != "" {
			return digest, nil
		}
		return "", fmt.Errorf("200 OK but no Docker-Content-Digest header")
	}

	// If 401, we need to do token exchange
	if resp.StatusCode == http.StatusUnauthorized {
		challenge := resp.Header.Get("Www-Authenticate")
		if challenge == "" {
			return "", fmt.Errorf("401 with no Www-Authenticate header")
		}

		token, err := exchangeToken(challenge, ref)
		if err != nil {
			return "", fmt.Errorf("token exchange: %w", err)
		}

		// Retry with Bearer token
		req2, err := http.NewRequest("HEAD", manifestURL, http.NoBody)
		if err != nil {
			return "", fmt.Errorf("creating authenticated request: %w", err)
		}
		for _, accept := range acceptHeaders {
			req2.Header.Add("Accept", accept)
		}
		req2.Header.Set("Authorization", "Bearer "+token)

		resp2, err := client.Do(req2)
		if err != nil {
			return "", fmt.Errorf("authenticated HEAD manifest: %w", err)
		}
		resp2.Body.Close()

		if resp2.StatusCode != http.StatusOK {
			return "", fmt.Errorf("authenticated HEAD returned %d", resp2.StatusCode)
		}

		digest := resp2.Header.Get("Docker-Content-Digest")
		if digest != "" {
			return digest, nil
		}
		return "", fmt.Errorf("authenticated 200 OK but no Docker-Content-Digest header")
	}

	return "", fmt.Errorf("unexpected status %d from manifest HEAD", resp.StatusCode)
}

// tokenResponse is the JSON structure returned by token endpoints.
type tokenResponse struct {
	Token       string `json:"token"`
	AccessToken string `json:"access_token"`
}

// exchangeToken performs the OAuth2 token exchange using the Www-Authenticate
// challenge parameters. For public images this uses anonymous auth; for private
// images it uses Basic auth with credentials from ~/.docker/config.json.
func exchangeToken(challenge string, ref ImageRef) (string, error) {
	// Parse "Bearer realm=...,service=...,scope=..."
	params := parseChallenge(challenge)

	realm := params["realm"]
	if realm == "" {
		return "", fmt.Errorf("no realm in challenge: %s", challenge)
	}

	// Build token request URL
	tokenURL := realm + "?"
	if service := params["service"]; service != "" {
		tokenURL += "service=" + service + "&"
	}
	if scope := params["scope"]; scope != "" {
		tokenURL += "scope=" + scope
	} else {
		// Default scope for pulling
		tokenURL += "scope=repository:" + ref.Repository + ":pull"
	}

	req, err := http.NewRequest("GET", tokenURL, http.NoBody)
	if err != nil {
		return "", fmt.Errorf("creating token request: %w", err)
	}

	// Add Basic auth for private registries
	if username, password, ok := credentialsForRegistry(ref.Registry); ok {
		req.SetBasicAuth(username, password)
	}

	client := &http.Client{}
	resp, err := client.Do(req)
	if err != nil {
		return "", fmt.Errorf("token request: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("token endpoint returned %d", resp.StatusCode)
	}

	var tokenResp tokenResponse
	if err := json.NewDecoder(resp.Body).Decode(&tokenResp); err != nil {
		return "", fmt.Errorf("decoding token response: %w", err)
	}

	// Some registries use "token", others use "access_token"
	token := tokenResp.Token
	if token == "" {
		token = tokenResp.AccessToken
	}
	if token == "" {
		return "", fmt.Errorf("empty token in response")
	}

	return token, nil
}

// parseChallenge parses a Www-Authenticate header value like:
// `Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull"`
func parseChallenge(header string) map[string]string {
	params := map[string]string{}

	// Strip "Bearer " prefix
	header = strings.TrimSpace(header)
	if strings.HasPrefix(header, "Bearer ") {
		header = header[7:]
	} else if strings.HasPrefix(header, "bearer ") {
		header = header[7:]
	}

	// Parse key=value pairs
	for _, part := range splitChallengeParts(header) {
		part = strings.TrimSpace(part)
		eq := strings.Index(part, "=")
		if eq < 0 {
			continue
		}
		key := strings.TrimSpace(part[:eq])
		val := strings.TrimSpace(part[eq+1:])
		// Strip quotes
		if len(val) >= 2 && val[0] == '"' && val[len(val)-1] == '"' {
			val = val[1 : len(val)-1]
		}
		params[key] = val
	}

	return params
}

// splitChallengeParts splits on commas that are not inside quotes.
func splitChallengeParts(s string) []string {
	var parts []string
	var current strings.Builder
	inQuotes := false

	for _, r := range s {
		switch {
		case r == '"':
			inQuotes = !inQuotes
			current.WriteRune(r)
		case r == ',' && !inQuotes:
			parts = append(parts, current.String())
			current.Reset()
		default:
			current.WriteRune(r)
		}
	}

	if current.Len() > 0 {
		parts = append(parts, current.String())
	}

	return parts
}

// dockerConfig represents the structure of ~/.docker/config.json.
type dockerConfig struct {
	Auths map[string]dockerAuthEntry `json:"auths"`
}

type dockerAuthEntry struct {
	Auth string `json:"auth"`
}

// credentialsForRegistry reads ~/.docker/config.json and returns username/password
// for the given registry, or ok=false if not found.
func credentialsForRegistry(registry string) (username, password string, ok bool) {
	configPath := "/root/.docker/config.json"
	if v := os.Getenv("DOCKER_CONFIG"); v != "" {
		configPath = v + "/config.json"
	}

	data, err := os.ReadFile(configPath)
	if err != nil {
		return "", "", false
	}

	var cfg dockerConfig
	if err := json.Unmarshal(data, &cfg); err != nil {
		return "", "", false
	}

	// Map registry-1.docker.io lookups back to the keys used in config.json
	lookupKeys := registryConfigKeys(registry)

	for _, key := range lookupKeys {
		if entry, found := cfg.Auths[key]; found && entry.Auth != "" {
			decoded, err := base64.StdEncoding.DecodeString(entry.Auth)
			if err != nil {
				continue
			}
			parts := strings.SplitN(string(decoded), ":", 2)
			if len(parts) == 2 {
				return parts[0], parts[1], true
			}
		}
	}

	return "", "", false
}

// registryConfigKeys returns the set of keys to try when looking up
// credentials in ~/.docker/config.json for a given registry hostname.
func registryConfigKeys(registry string) []string {
	keys := []string{
		registry,
		"https://" + registry,
		"https://" + registry + "/v1/",
		"https://" + registry + "/v2/",
	}

	// Docker Hub has many aliases
	if registry == "registry-1.docker.io" {
		keys = append(keys,
			"docker.io",
			"https://docker.io",
			"index.docker.io",
			"https://index.docker.io",
			"https://index.docker.io/v1/",
			"https://index.docker.io/v2/",
		)
	}

	return keys
}

// AuthForImage returns a base64-encoded JSON auth string suitable for
// Docker Engine API calls (ImagePull). Returns empty string if no credentials found.
// This is the Docker Engine auth format, not the registry Bearer token.
func AuthForImage(imageRef string) string {
	ref := ParseImageRef(imageRef)
	username, password, ok := credentialsForRegistry(ref.Registry)
	if !ok {
		return ""
	}

	authConfig := struct {
		Username string `json:"username"`
		Password string `json:"password"`
	}{
		Username: username,
		Password: password,
	}

	encoded, err := json.Marshal(authConfig)
	if err != nil {
		return ""
	}
	return base64.URLEncoding.EncodeToString(encoded)
}
