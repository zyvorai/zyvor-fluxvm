// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package fluxvm

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func sessionServer(t *testing.T) (*Client, *[]string) {
	t.Helper()
	var calls []string
	info := `{"id":"s1","agent":"codex","sandbox_id":"b1","backend":"vz","children":[],"state":"running",
	  "capabilities":{"snapshot":true,"fork":false,"pty":false},
	  "policy":{"preset":"offline","offline":true,"allow_hosts":[],"unenforced":["process"]},
	  "workspace":{"path":"/workspace","git":null,"mode":"clone","cloned":false,"baseline":true,"note":null},
	  "checkpoints":[]}`
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		calls = append(calls, r.Method+" "+r.URL.RequestURI()+" "+string(body))
		w.Header().Set("Content-Type", "application/json")
		switch {
		case r.Method == "POST" && r.URL.Path == "/v1/agents/sessions":
			w.WriteHeader(201)
			io.WriteString(w, info)
		case r.Method == "GET" && r.URL.Path == "/v1/agents/sessions":
			io.WriteString(w, `{"items":[`+info+`]}`)
		case r.Method == "GET" && r.URL.Path == "/v1/agents/sessions/s1":
			io.WriteString(w, info)
		case strings.HasSuffix(r.URL.Path, "/exec"):
			io.WriteString(w, `{"result":"exec","exit_code":3,"stdout":"out","stderr":"err"}`)
		case strings.HasSuffix(r.URL.Path, "/checkpoint"):
			io.WriteString(w, `{"tag":"cp-1","memory":false,"created_at":"2026-10-13T00:00:00Z"}`)
		case strings.HasSuffix(r.URL.Path, "/restore"):
			io.WriteString(w, `{"restored":"cp-1"}`)
		case strings.HasSuffix(r.URL.Path, "/fork"):
			w.WriteHeader(409)
			io.WriteString(w, `{"error":"fork is not available on backend vz"}`)
		case strings.HasSuffix(r.URL.Path, "/events"):
			io.WriteString(w, `{"ledger":[{"kind":"exec"}],"vm_events":[]}`)
		case strings.HasSuffix(r.URL.Path, "/artifacts"):
			io.WriteString(w, `{"changes":null,"note":"no baseline"}`)
		case r.Method == "DELETE":
			w.WriteHeader(204)
		default:
			w.WriteHeader(404)
		}
	}))
	t.Cleanup(srv.Close)
	return NewClient(srv.URL), &calls
}

func TestSessionLifecycle(t *testing.T) {
	c, calls := sessionServer(t)
	ctx := context.Background()
	s, err := c.CreateSession(ctx, CreateSessionRequest{
		Agent:     "codex",
		Policy:    &SessionPolicyRequest{Preset: "offline"},
		Workspace: &SessionWorkspaceRequest{Git: "https://github.com/octocat/Hello-World"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if s.Info.ID != "s1" || s.Info.Capabilities.Fork || !s.Info.Capabilities.Snapshot {
		t.Fatalf("info %+v", s.Info)
	}
	if got := (*calls)[0]; !strings.Contains(got, `"preset":"offline"`) || !strings.Contains(got, `"git":"https://github.com/octocat/Hello-World"`) {
		t.Fatalf("create body: %s", got)
	}
	res, err := s.Exec(ctx, "false", 10)
	if err != nil || res.ExitCode != 3 || res.Stdout != "out" {
		t.Fatalf("exec %+v %v", res, err)
	}
	if got := (*calls)[1]; !strings.Contains(got, `"timeout_seconds":10`) {
		t.Fatalf("exec body: %s", got)
	}
	cp, err := s.Checkpoint(ctx, "", false)
	if err != nil || cp.Tag != "cp-1" {
		t.Fatalf("checkpoint %+v %v", cp, err)
	}
	if got := (*calls)[2]; !strings.Contains(got, `"memory":false`) || strings.Contains(got, `"tag"`) {
		t.Fatalf("checkpoint body: %s", got)
	}
	if err := s.Restore(ctx, "cp-1"); err != nil {
		t.Fatal(err)
	}
	ev, err := s.Events(ctx, "2026-10-13T00:00:00Z")
	if err != nil || len(ev.Ledger) != 1 {
		t.Fatalf("events %+v %v", ev, err)
	}
	if got := (*calls)[4]; !strings.Contains(got, "since=2026-10-13T00%3A00%3A00Z") {
		t.Fatalf("events query: %s", got)
	}
	art, err := s.Artifacts(ctx)
	var a struct{ Note string }
	if err != nil || json.Unmarshal(art, &a) != nil || a.Note != "no baseline" {
		t.Fatalf("artifacts %s %v", art, err)
	}
	list, err := c.ListSessions(ctx)
	if err != nil || len(list) != 1 {
		t.Fatalf("list %v %v", list, err)
	}
	if err := s.Delete(ctx); err != nil {
		t.Fatal(err)
	}
}

func TestSessionForkRefusalIsATypedError(t *testing.T) {
	c, _ := sessionServer(t)
	s, err := c.GetSession(context.Background(), "s1")
	if err != nil {
		t.Fatal(err)
	}
	_, err = s.Fork(context.Background(), 2)
	var ae *APIError
	if !errors.As(err, &ae) || ae.Status != 409 || !strings.Contains(ae.Message, "not available on backend vz") {
		t.Fatalf("want a 409 APIError, got %v", err)
	}
}
