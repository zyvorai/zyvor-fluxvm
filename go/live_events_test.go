//go:build live

// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package fluxvm

import (
	"context"
	"os"
	"testing"
	"time"
)

// TestLiveWatchEvents needs FLUXVM_LIVE_EVENTS_URL: a daemon with a session template named "fc".
// Run with: FLUXVM_LIVE_EVENTS_URL=http://127.0.0.1:7788 go test -tags live -run LiveWatchEvents .
func TestLiveWatchEvents(t *testing.T) {
	base := os.Getenv("FLUXVM_LIVE_EVENTS_URL")
	if base == "" {
		t.Skip("FLUXVM_LIVE_EVENTS_URL not set")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()
	c := NewClient(base)
	var info SessionInfo
	if err := c.call(ctx, "POST", "/v1/agents/sessions", map[string]any{"agent": "custom", "runtime": map[string]any{"template": "fc"}}, &info, 4*time.Minute); err != nil {
		t.Fatal(err)
	}
	s := &Session{client: c, Info: info}
	var kinds []string
	done := make(chan error, 1)
	go func() {
		done <- s.WatchEvents(ctx, WatchOptions{}, func(e SessionStreamEvent) error {
			kinds = append(kinds, e.Type)
			return nil
		})
	}()
	var out *ExecResult
	var err error
	for i := 0; i < 30; i++ {
		if out, err = s.Exec(ctx, "echo go-live", 10); err == nil {
			break
		}
		time.Sleep(time.Second)
	}
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("exec: %q", out.Stdout)
	if err := s.Delete(ctx); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-done:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(30 * time.Second):
		t.Fatal("stream did not end after delete")
	}
	t.Logf("events: %v", kinds)
	if len(kinds) < 3 || kinds[len(kinds)-1] != "error" {
		t.Fatalf("kinds = %v", kinds)
	}
}
