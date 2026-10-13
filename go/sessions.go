// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package fluxvm

import (
	"context"
	"encoding/json"
	"net/http"
	"net/url"
	"time"
)

// SessionCapabilities says what a session can do on its backend. Fork is true only on the
// in-tree KVM engine; check it instead of calling Fork and handling the 409.
type SessionCapabilities struct {
	Snapshot bool `json:"snapshot"`
	Fork     bool `json:"fork"`
	PTY      bool `json:"pty"`
}

// SessionPolicy is the compiled network policy of a session. Unenforced lists policy sections
// that were accepted and recorded but that nothing enforces yet.
type SessionPolicy struct {
	Preset     *string  `json:"preset"`
	Offline    bool     `json:"offline"`
	AllowHosts []string `json:"allow_hosts"`
	Unenforced []string `json:"unenforced"`
}

// SessionCheckpoint is one snapshot of a session.
type SessionCheckpoint struct {
	Tag       string `json:"tag"`
	Memory    bool   `json:"memory"`
	CreatedAt string `json:"created_at"`
}

// SessionWorkspace describes the session's workspace directory.
type SessionWorkspace struct {
	Path     string  `json:"path"`
	Git      *string `json:"git"`
	Mode     string  `json:"mode"`
	Cloned   bool    `json:"cloned"`
	Baseline bool    `json:"baseline"`
	Note     *string `json:"note"`
}

// SessionInfo is the server's record of an agent session.
type SessionInfo struct {
	ID           string              `json:"id"`
	Agent        string              `json:"agent"`
	SandboxID    string              `json:"sandbox_id"`
	Backend      string              `json:"backend"`
	Parent       *string             `json:"parent"`
	Children     []string            `json:"children"`
	State        string              `json:"state"` // running | deleted
	CreatedAt    string              `json:"created_at"`
	Capabilities SessionCapabilities `json:"capabilities"`
	Policy       SessionPolicy       `json:"policy"`
	Workspace    *SessionWorkspace   `json:"workspace"`
	Checkpoints  []SessionCheckpoint `json:"checkpoints"`
}

// SessionWorkspaceRequest asks for a workspace. Git is cloned into Path (default /workspace);
// the image needs git and the policy must allow the host.
type SessionWorkspaceRequest struct {
	Git  string `json:"git,omitempty"`
	Mode string `json:"mode,omitempty"` // clone (default) or readonly
	Path string `json:"path,omitempty"`
}

// SessionRuntime sizes the session's sandbox.
type SessionRuntime struct {
	Image    string `json:"image,omitempty"`
	CPUs     uint8  `json:"cpus,omitempty"`
	MemoryMB uint64 `json:"memory_mb,omitempty"`
}

// SessionNetwork is a deny-by-default host allow-list.
type SessionNetwork struct {
	Default string   `json:"default,omitempty"` // deny (default) or allow
	Allow   []string `json:"allow,omitempty"`   // host names, e.g. "github.com", "*.pypi.org"
}

// SessionPolicyRequest picks a preset (offline, developer-safe, open) and/or a host allow-list.
// Filesystem and Process are stored and reported unenforced.
type SessionPolicyRequest struct {
	Preset     string          `json:"preset,omitempty"`
	Network    *SessionNetwork `json:"network,omitempty"`
	Filesystem json.RawMessage `json:"filesystem,omitempty"`
	Process    json.RawMessage `json:"process,omitempty"`
}

// CreateSessionRequest is the body of POST /v1/agents/sessions.
type CreateSessionRequest struct {
	Agent      string                   `json:"agent,omitempty"`
	Workspace  *SessionWorkspaceRequest `json:"workspace,omitempty"`
	Runtime    *SessionRuntime          `json:"runtime,omitempty"`
	Policy     *SessionPolicyRequest    `json:"policy,omitempty"`
	TTLSeconds uint64                   `json:"ttl_seconds,omitempty"`
}

// Session is a handle on one agent session.
type Session struct {
	client *Client
	Info   SessionInfo
}

func (s *Session) path(suffix string) string {
	return "/v1/agents/sessions/" + url.PathEscape(s.Info.ID) + suffix
}

// CreateSession calls POST /v1/agents/sessions (needs an admin token). It returns once the
// sandbox can run commands and, when a repository was asked for, the clone has been tried:
// check Info.Workspace.Cloned and Note.
func (c *Client) CreateSession(ctx context.Context, req CreateSessionRequest) (*Session, error) {
	var info SessionInfo
	if err := c.call(ctx, http.MethodPost, "/v1/agents/sessions", req, &info, 5*time.Minute, http.StatusCreated); err != nil {
		return nil, err
	}
	return &Session{client: c, Info: info}, nil
}

// GetSession calls GET /v1/agents/sessions/{id}.
func (c *Client) GetSession(ctx context.Context, id string) (*Session, error) {
	s := &Session{client: c, Info: SessionInfo{ID: id}}
	if err := s.Refresh(ctx); err != nil {
		return nil, err
	}
	return s, nil
}

// ListSessions calls GET /v1/agents/sessions: the caller's sessions, oldest first.
func (c *Client) ListSessions(ctx context.Context) ([]SessionInfo, error) {
	var out struct {
		Items []SessionInfo `json:"items"`
	}
	if err := c.call(ctx, http.MethodGet, "/v1/agents/sessions", nil, &out, 0); err != nil {
		return nil, err
	}
	return out.Items, nil
}

// Refresh reloads the record (checkpoints, children, state).
func (s *Session) Refresh(ctx context.Context) error {
	var info SessionInfo
	if err := s.client.call(ctx, http.MethodGet, s.path(""), nil, &info, 0); err != nil {
		return err
	}
	s.Info = info
	return nil
}

// Exec runs a shell string in the session; a non-zero exit is not an error. The call is
// recorded in the session's ledger.
func (s *Session) Exec(ctx context.Context, command string, timeoutSeconds int) (*ExecResult, error) {
	payload := map[string]interface{}{"command": command}
	limit := DefaultExecTimeout
	if timeoutSeconds > 0 {
		payload["timeout_seconds"] = timeoutSeconds
		limit = time.Duration(timeoutSeconds) * time.Second
	}
	var out ExecResult
	if err := s.client.call(ctx, http.MethodPost, s.path("/exec"), payload, &out, limit+execHTTPSlack); err != nil {
		return nil, err
	}
	return &out, nil
}

// Checkpoint snapshots the session under tag (empty = server-chosen). memory=false takes a
// disk-only snapshot, which is much smaller but cold-boots on restore.
func (s *Session) Checkpoint(ctx context.Context, tag string, memory bool) (*SessionCheckpoint, error) {
	payload := map[string]interface{}{}
	if tag != "" {
		payload["tag"] = tag
	}
	if !memory {
		payload["memory"] = false
	}
	var cp SessionCheckpoint
	if err := s.client.call(ctx, http.MethodPost, s.path("/checkpoint"), payload, &cp, 5*time.Minute); err != nil {
		return nil, err
	}
	return &cp, nil
}

// Restore returns the session to a checkpoint, discarding everything since.
func (s *Session) Restore(ctx context.Context, tag string) error {
	return s.client.call(ctx, http.MethodPost, s.path("/restore"), map[string]string{"tag": tag}, nil, 5*time.Minute)
}

// Fork creates count child sessions from the current state. It fails with a 409 APIError,
// and creates nothing, when the backend cannot fork (check Info.Capabilities.Fork first).
func (s *Session) Fork(ctx context.Context, count int) ([]SessionInfo, error) {
	var out struct {
		Items []SessionInfo `json:"items"`
	}
	if err := s.client.call(ctx, http.MethodPost, s.path("/fork"), map[string]int{"count": count}, &out, 5*time.Minute, http.StatusCreated); err != nil {
		return nil, err
	}
	return out.Items, nil
}

// SessionEvents is the session's ledger plus the sandbox's VM events. Entries are raw JSON.
type SessionEvents struct {
	Ledger   []json.RawMessage `json:"ledger"`
	VMEvents []json.RawMessage `json:"vm_events"`
}

// Events calls GET .../events. since is an RFC 3339 time or "" for everything.
func (s *Session) Events(ctx context.Context, since string) (*SessionEvents, error) {
	p := s.path("/events")
	if since != "" {
		p += "?since=" + url.QueryEscape(since)
	}
	var out SessionEvents
	if err := s.client.call(ctx, http.MethodGet, p, nil, &out, 0); err != nil {
		return nil, err
	}
	return &out, nil
}

// Artifacts calls GET .../artifacts: the workspace changes since the baseline (Changes is
// null when there is no baseline; Note says why), checkpoints, children and counters.
func (s *Session) Artifacts(ctx context.Context) (json.RawMessage, error) {
	var raw json.RawMessage
	if err := s.client.call(ctx, http.MethodGet, s.path("/artifacts"), nil, &raw, 0); err != nil {
		return nil, err
	}
	return raw, nil
}

// Delete ends the session and deletes its sandbox. It is safe to call twice.
func (s *Session) Delete(ctx context.Context) error {
	return s.client.call(ctx, http.MethodDelete, s.path(""), nil, nil, 2*time.Minute, http.StatusNoContent)
}
