// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package fleet

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strconv"
	"strings"
	"time"
)

// Sentinel errors for errors.Is.
var (
	// ErrUnauthorized matches a 401: no token, or one central does not know.
	ErrUnauthorized = errors.New("fleet: missing or invalid token")
	// ErrForbidden matches a 403: a tenant calling an operator route, a read-only
	// key changing something, or a disabled tenant.
	ErrForbidden = errors.New("fleet: forbidden")
	// ErrNotFound matches a 404. Central also answers 404 for another tenant's resources.
	ErrNotFound = errors.New("fleet: not found")
	// ErrConflict matches a 409 (a Mac already leased, a lease released too early, ...).
	ErrConflict = errors.New("fleet: conflict")
	// ErrQuota matches a 429 (a tenant's quota, or rate limiting).
	ErrQuota = errors.New("fleet: quota exceeded or rate limited")
	// ErrUnavailable matches a 503 (no Mac can take it, or this central is a standby).
	ErrUnavailable = errors.New("fleet: unavailable")
	// ErrTimeout matches a request that hit the client timeout or a context deadline.
	ErrTimeout = errors.New("fleet: request timed out")
	// ErrConnection matches a failure to reach central.
	ErrConnection = errors.New("fleet: connection failed")
)

// APIError is a non-2xx answer from central. Message is central's
// {"error": "..."} text, or the raw body when it had another shape.
type APIError struct {
	Status  int
	Message string
	Body    []byte
	// RetryAfter is the Retry-After header, when central sent one.
	RetryAfter time.Duration
}

func (e *APIError) Error() string {
	return fmt.Sprintf("fleet: HTTP %d: %s", e.Status, e.Message)
}

// Is maps the status onto the sentinel errors.
func (e *APIError) Is(target error) bool {
	switch target {
	case ErrUnauthorized:
		return e.Status == http.StatusUnauthorized
	case ErrForbidden:
		return e.Status == http.StatusForbidden
	case ErrNotFound:
		return e.Status == http.StatusNotFound
	case ErrConflict:
		return e.Status == http.StatusConflict
	case ErrQuota:
		return e.Status == http.StatusTooManyRequests
	case ErrUnavailable:
		return e.Status == http.StatusServiceUnavailable
	}
	return false
}

// IsNotFound reports whether err is a 404 from central.
func IsNotFound(err error) bool { return errors.Is(err, ErrNotFound) }

// StatusOf returns the HTTP status of an *APIError in err's chain, or 0.
func StatusOf(err error) int {
	var ae *APIError
	if errors.As(err, &ae) {
		return ae.Status
	}
	return 0
}

func newAPIError(status int, hdr http.Header, body []byte) *APIError {
	e := &APIError{Status: status, Body: body}
	var shaped struct {
		Error string `json:"error"`
	}
	if json.Unmarshal(body, &shaped) == nil && shaped.Error != "" {
		e.Message = shaped.Error
	} else {
		e.Message = strings.TrimSpace(string(body))
		if len(e.Message) > 512 {
			e.Message = e.Message[:512] + "…"
		}
	}
	if e.Message == "" {
		e.Message = http.StatusText(status)
	}
	if hdr != nil {
		if ra := hdr.Get("Retry-After"); ra != "" {
			if secs, err := strconv.Atoi(ra); err == nil && secs >= 0 {
				e.RetryAfter = time.Duration(secs) * time.Second
			} else if t, err := http.ParseTime(ra); err == nil {
				if d := time.Until(t); d > 0 {
					e.RetryAfter = d
				}
			}
		}
	}
	return e
}
