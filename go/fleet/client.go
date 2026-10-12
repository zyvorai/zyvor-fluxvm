// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

// Package fleet is a client for FluxVM central's fleet API (/fleet/*): the Mac
// cloud's tenants, API keys, Mac leases, VMs, private networks, the image
// catalog, webhooks, prices, billing, budgets, the activity log, volumes and
// warm pools.
//
// The node-level sandbox API (/v1/*) lives in the parent package; this one
// talks to `fluxvm-agent central`.
//
//	c := fleet.NewClient("https://fleet.example.com", fleet.WithToken(os.Getenv("FLUXVM_TOKEN")))
//	lease, err := c.CreateLease(ctx, fleet.CreateLeaseRequest{Hours: fleet.Int64(48)})
//	vm, err := c.LaunchVM(ctx, fleet.LaunchRequest{Name: "ci-1", Image: "/Users/fluxvm/vms/xcode/disk.raw",
//		VCPUs: 6, MemoryMiB: 16384, Apple: &fleet.AppleOptions{GuestOS: "macos"}})
//
// Every method takes a context. Errors from the server are *APIError (status
// and central's message); errors.Is(err, fleet.ErrNotFound) and friends work
// on them. Idempotent calls (GET, PUT, DELETE) are retried on 429 and 5xx with
// backoff, honouring Retry-After; POSTs are never retried.
package fleet

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math/rand"
	"net"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"
)

// Version is sent in the User-Agent header.
const Version = "0.1.0"

// DefaultTimeout is the per-attempt timeout when WithTimeout is not given.
const DefaultTimeout = 60 * time.Second

// maxBody bounds how much of a response is read (a VM list from a big fleet is
// the largest answer central gives).
const maxBody = 64 << 20

// Client talks to one central. It is safe for concurrent use.
type Client struct {
	baseURL    string
	token      string
	userAgent  string
	timeout    time.Duration
	retries    int
	retryBase  time.Duration
	retryMax   time.Duration
	httpClient *http.Client
}

// Option configures a Client.
type Option func(*Client)

// WithToken sets the bearer token: the fleet (operator) token, a tenant's
// primary token or a named API key.
func WithToken(token string) Option { return func(c *Client) { c.token = token } }

// WithHTTPClient supplies the underlying http.Client (custom TLS, proxy). It
// is copied; the copy does not follow redirects.
func WithHTTPClient(h *http.Client) Option {
	return func(c *Client) {
		if h != nil {
			cp := *h
			c.httpClient = &cp
		}
	}
}

// WithTimeout sets the per-attempt timeout. A sooner context deadline wins.
func WithTimeout(d time.Duration) Option { return func(c *Client) { c.timeout = d } }

// WithRetries sets how many times an idempotent call is retried after a 429,
// a 5xx or a connection failure (default 3; 0 turns retries off).
func WithRetries(n int) Option {
	return func(c *Client) {
		if n >= 0 {
			c.retries = n
		}
	}
}

// WithRetryDelay sets the first backoff delay and its cap (defaults 500ms and
// 10s). Each retry doubles the delay, with jitter; a Retry-After header wins
// when it is longer, up to the cap.
func WithRetryDelay(base, max time.Duration) Option {
	return func(c *Client) {
		if base > 0 {
			c.retryBase = base
		}
		if max > 0 {
			c.retryMax = max
		}
	}
}

// WithUserAgent replaces the User-Agent header (for example "terraform-provider-fluxvm/0.1.0").
func WithUserAgent(ua string) Option { return func(c *Client) { c.userAgent = ua } }

// NewClient returns a Client for central's base URL, e.g. "http://fleet:7799".
func NewClient(baseURL string, opts ...Option) *Client {
	c := &Client{
		baseURL:    strings.TrimRight(baseURL, "/"),
		userAgent:  "fluxvm-fleet-go/" + Version,
		timeout:    DefaultTimeout,
		retries:    3,
		retryBase:  500 * time.Millisecond,
		retryMax:   10 * time.Second,
		httpClient: &http.Client{},
	}
	for _, o := range opts {
		o(c)
	}
	c.httpClient.CheckRedirect = func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }
	return c
}

// BaseURL is the central this client talks to.
func (c *Client) BaseURL() string { return c.baseURL }

func idempotent(method string) bool {
	switch method {
	case http.MethodGet, http.MethodHead, http.MethodPut, http.MethodDelete, http.MethodOptions:
		return true
	}
	return false
}

func retryable(status int) bool {
	return status == http.StatusTooManyRequests || status >= 500
}

// raw performs a request with retries and returns the final status, headers and body.
func (c *Client) raw(ctx context.Context, method, path string, payload interface{}, accept string) (int, http.Header, []byte, error) {
	if c.baseURL == "" {
		return 0, nil, nil, errors.New("fleet: the central URL is required")
	}
	var body []byte
	if payload != nil {
		b, err := json.Marshal(payload)
		if err != nil {
			return 0, nil, nil, fmt.Errorf("fleet: encoding request: %w", err)
		}
		body = b
	}
	attempts := 1
	if idempotent(method) {
		attempts += c.retries
	}
	var lastErr error
	for i := 0; i < attempts; i++ {
		if i > 0 {
			if err := sleep(ctx, c.backoff(i, lastErr)); err != nil {
				return 0, nil, nil, err
			}
		}
		status, hdr, data, err := c.once(ctx, method, path, body, accept)
		if err != nil {
			if errors.Is(err, context.Canceled) || ctx.Err() != nil {
				return 0, nil, nil, err
			}
			lastErr = err
			continue
		}
		if retryable(status) && i+1 < attempts {
			lastErr = newAPIError(status, hdr, data)
			continue
		}
		return status, hdr, data, nil
	}
	return 0, nil, nil, lastErr
}

func (c *Client) backoff(attempt int, last error) time.Duration {
	d := c.retryBase << uint(attempt-1)
	if d > c.retryMax || d <= 0 {
		d = c.retryMax
	}
	// Up to 25% jitter so a fleet of scripts does not retry in lockstep.
	d += time.Duration(rand.Int63n(int64(d)/4 + 1))
	var ae *APIError
	if errors.As(last, &ae) && ae.RetryAfter > d {
		d = ae.RetryAfter
		if d > c.retryMax {
			d = c.retryMax
		}
	}
	return d
}

func sleep(ctx context.Context, d time.Duration) error {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-t.C:
		return nil
	}
}

func (c *Client) once(ctx context.Context, method, path string, body []byte, accept string) (int, http.Header, []byte, error) {
	ctx, cancel := context.WithTimeout(ctx, c.timeout)
	defer cancel()
	var reader io.Reader
	if body != nil {
		reader = bytes.NewReader(body)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.baseURL+path, reader)
	if err != nil {
		return 0, nil, nil, fmt.Errorf("fleet: building request: %w", err)
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if accept == "" {
		accept = "application/json"
	}
	req.Header.Set("Accept", accept)
	req.Header.Set("User-Agent", c.userAgent)
	if c.token != "" {
		req.Header.Set("Authorization", "Bearer "+c.token)
	}
	resp, err := c.httpClient.Do(req)
	if err != nil {
		return 0, nil, nil, classify(ctx, method, path, err)
	}
	defer resp.Body.Close()
	data, err := io.ReadAll(io.LimitReader(resp.Body, maxBody))
	if err != nil {
		return 0, nil, nil, classify(ctx, method, path, err)
	}
	return resp.StatusCode, resp.Header, data, nil
}

func classify(ctx context.Context, method, path string, err error) error {
	if errors.Is(err, context.Canceled) {
		return err
	}
	var ne net.Error
	if errors.Is(err, context.DeadlineExceeded) || ctx.Err() == context.DeadlineExceeded || (errors.As(err, &ne) && ne.Timeout()) {
		return fmt.Errorf("%w: %s %s", ErrTimeout, method, path)
	}
	return fmt.Errorf("%w: %s %s: %v", ErrConnection, method, path, err)
}

// call sends payload (nil for none) and decodes a 2xx JSON answer into out (nil to ignore it).
func (c *Client) call(ctx context.Context, method, path string, payload, out interface{}) error {
	status, hdr, data, err := c.raw(ctx, method, path, payload, "")
	if err != nil {
		return err
	}
	if status < 200 || status > 299 {
		return newAPIError(status, hdr, data)
	}
	if out == nil || len(data) == 0 {
		return nil
	}
	if r, ok := out.(*json.RawMessage); ok {
		*r = append((*r)[:0], data...)
		return nil
	}
	if err := json.Unmarshal(data, out); err != nil {
		return &APIError{Status: status, Message: "the answer was not the expected JSON: " + err.Error(), Body: data}
	}
	return nil
}

// text fetches a non-JSON answer (CSV, logs).
func (c *Client) text(ctx context.Context, path, accept string) (string, error) {
	status, hdr, data, err := c.raw(ctx, http.MethodGet, path, nil, accept)
	if err != nil {
		return "", err
	}
	if status < 200 || status > 299 {
		return "", newAPIError(status, hdr, data)
	}
	return string(data), nil
}

// Do sends an arbitrary request to central, for routes this package does not
// wrap yet. payload is JSON-encoded when not nil; a 2xx JSON answer is decoded
// into out when not nil.
func (c *Client) Do(ctx context.Context, method, path string, payload, out interface{}) error {
	return c.call(ctx, method, path, payload, out)
}

// seg escapes one path segment.
func seg(s string) string { return url.PathEscape(s) }

// query builds "?k=v&..." from non-empty values (empty string when none).
func query(kv ...string) string {
	q := url.Values{}
	for i := 0; i+1 < len(kv); i += 2 {
		if kv[i+1] != "" {
			q.Set(kv[i], kv[i+1])
		}
	}
	if len(q) == 0 {
		return ""
	}
	return "?" + q.Encode()
}

func itoa(n int) string {
	if n <= 0 {
		return ""
	}
	return strconv.Itoa(n)
}
