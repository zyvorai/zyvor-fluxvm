// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package fluxvm

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"sync"
	"testing"
	"time"
)

func sseFrame(w http.ResponseWriter, event, id, data string) {
	fmt.Fprintf(w, "event: %s\nid: %s\ndata: %s\n\n", event, id, data)
}

type eventsServer struct {
	mu        sync.Mutex
	paths     []string
	lastIDs   []string
	dropFirst bool
}

func (e *eventsServer) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	e.mu.Lock()
	e.paths = append(e.paths, r.URL.RequestURI())
	e.lastIDs = append(e.lastIDs, r.Header.Get("Last-Event-ID"))
	drop := e.dropFirst
	if drop {
		e.dropFirst = false
	}
	e.mu.Unlock()
	switch r.URL.Path {
	case "/v1/agents/sessions/missing/events/stream":
		w.WriteHeader(http.StatusNotFound)
		fmt.Fprint(w, `{"error":"session not found","code":"NOT_FOUND"}`)
		return
	}
	w.Header().Set("Content-Type", "text/event-stream")
	resume := r.Header.Get("Last-Event-ID")
	if resume == "" {
		sseFrame(w, "ledger", "1|", `{"op":"create"}`)
		if drop {
			return
		}
		sseFrame(w, "vm", "1|t", `{"event":"vm.start"}`)
		sseFrame(w, "ledger", "2|t", `{"op":"exec"}`)
	} else {
		sseFrame(w, "ledger", "2|t", `{"op":"exec"}`)
	}
	sseFrame(w, "error", "2|t", `{"error":"session not found"}`)
}

func watchSession(t *testing.T, srv *httptest.Server, id string, opts WatchOptions) ([]SessionStreamEvent, error) {
	t.Helper()
	c := NewClient(srv.URL, WithToken("t"))
	s := &Session{client: c, Info: SessionInfo{ID: id}}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	var got []SessionStreamEvent
	err := s.WatchEvents(ctx, opts, func(e SessionStreamEvent) error {
		got = append(got, e)
		return nil
	})
	return got, err
}

func TestWatchEventsReplaysThenEndsOnError(t *testing.T) {
	es := &eventsServer{}
	srv := httptest.NewServer(es)
	defer srv.Close()
	got, err := watchSession(t, srv, "s1", WatchOptions{})
	if err != nil {
		t.Fatal(err)
	}
	var types []string
	for _, e := range got {
		types = append(types, e.Type)
	}
	if fmt.Sprint(types) != "[ledger vm ledger error]" {
		t.Fatalf("types = %v", types)
	}
	if string(got[0].Data) != `{"op":"create"}` || got[0].ID != "1|" {
		t.Fatalf("first = %+v", got[0])
	}
}

func TestWatchEventsSinceInQuery(t *testing.T) {
	es := &eventsServer{}
	srv := httptest.NewServer(es)
	defer srv.Close()
	if _, err := watchSession(t, srv, "s1", WatchOptions{Since: "2026-10-17T09:00:00Z"}); err != nil {
		t.Fatal(err)
	}
	if want := "/v1/agents/sessions/s1/events/stream?since=2026-10-17T09%3A00%3A00Z"; es.paths[0] != want {
		t.Fatalf("path = %q, want %q", es.paths[0], want)
	}
}

func TestWatchEventsReconnectsWithLastEventID(t *testing.T) {
	es := &eventsServer{dropFirst: true}
	srv := httptest.NewServer(es)
	defer srv.Close()
	got, err := watchSession(t, srv, "s1", WatchOptions{ReconnectDelay: 10 * time.Millisecond})
	if err != nil {
		t.Fatal(err)
	}
	if len(es.lastIDs) != 2 || es.lastIDs[0] != "" || es.lastIDs[1] != "1|" {
		t.Fatalf("Last-Event-ID per connection = %q", es.lastIDs)
	}
	if len(got) != 3 || got[1].Type != "ledger" || string(got[1].Data) != `{"op":"exec"}` {
		t.Fatalf("got = %+v", got)
	}
}

func TestWatchEventsDoesNotRetryA404AndExposesCode(t *testing.T) {
	es := &eventsServer{}
	srv := httptest.NewServer(es)
	defer srv.Close()
	_, err := watchSession(t, srv, "missing", WatchOptions{})
	var api *APIError
	if !errors.As(err, &api) || api.Code != "NOT_FOUND" || !errors.Is(err, ErrNotFound) {
		t.Fatalf("err = %v", err)
	}
	if len(es.paths) != 1 {
		t.Fatalf("a 404 was retried: %d requests", len(es.paths))
	}
}

func TestWatchEventsStopsWhenContextEnds(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		sseFrame(w, "ledger", "1|", `{"op":"create"}`)
		w.(http.Flusher).Flush()
		<-r.Context().Done()
	}))
	defer srv.Close()
	c := NewClient(srv.URL)
	s := &Session{client: c, Info: SessionInfo{ID: "s1"}}
	ctx, cancel := context.WithCancel(context.Background())
	err := s.WatchEvents(ctx, WatchOptions{}, func(SessionStreamEvent) error {
		cancel()
		return nil
	})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("err = %v", err)
	}
}
