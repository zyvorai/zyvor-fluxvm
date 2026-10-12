// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package fleet

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"time"
)

type items[T any] struct {
	Items []T `json:"items"`
}

// Me is the tenant name the key and budget routes accept for the calling tenant.
const Me = "me"

// Health calls GET /healthz (no token needed).
func (c *Client) Health(ctx context.Context) error {
	return c.call(ctx, http.MethodGet, "/healthz", nil, nil)
}

// ---- tenants ----

// ListTenants is GET /fleet/tenants (operator), with what each tenant uses.
func (c *Client) ListTenants(ctx context.Context) ([]Tenant, error) {
	var out items[Tenant]
	err := c.call(ctx, http.MethodGet, "/fleet/tenants", nil, &out)
	return out.Items, err
}

// CreateTenant is POST /fleet/tenants (operator). Store the returned token: central keeps only its hash.
func (c *Client) CreateTenant(ctx context.Context, req CreateTenantRequest) (*CreatedTenant, error) {
	var out CreatedTenant
	if err := c.call(ctx, http.MethodPost, "/fleet/tenants", req, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// GetTenant is GET /fleet/tenants/{id} (operator).
func (c *Client) GetTenant(ctx context.Context, id string) (*Tenant, error) {
	var out Tenant
	if err := c.call(ctx, http.MethodGet, "/fleet/tenants/"+seg(id), nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// Me is GET /fleet/tenants/me: the calling tenant (a tenant token or key only).
func (c *Client) Me(ctx context.Context) (*Tenant, error) {
	var out Tenant
	if err := c.call(ctx, http.MethodGet, "/fleet/tenants/me", nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// UpdateTenant is PATCH /fleet/tenants/{id} (operator).
func (c *Client) UpdateTenant(ctx context.Context, id string, req UpdateTenantRequest) (*Tenant, error) {
	var out Tenant
	if err := c.call(ctx, http.MethodPatch, "/fleet/tenants/"+seg(id), req, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// DeleteTenant is DELETE /fleet/tenants/{id} (operator); refused (409) while it has active leases or VMs.
func (c *Client) DeleteTenant(ctx context.Context, id string) error {
	return c.call(ctx, http.MethodDelete, "/fleet/tenants/"+seg(id), nil, nil)
}

// RotateTenantToken is POST /fleet/tenants/{id}/rotate-token (operator). Named keys keep working.
func (c *Client) RotateTenantToken(ctx context.Context, id string) (*RotatedToken, error) {
	var out RotatedToken
	if err := c.call(ctx, http.MethodPost, "/fleet/tenants/"+seg(id)+"/rotate-token", nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// SetBudget is PUT /fleet/tenants/{id|me}/budget. nil (or 0) removes the budget.
func (c *Client) SetBudget(ctx context.Context, tenant string, monthly *float64) (*Tenant, error) {
	var out Tenant
	body := map[string]interface{}{"monthly": monthly}
	if err := c.call(ctx, http.MethodPut, "/fleet/tenants/"+seg(tenant)+"/budget", body, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// ---- API keys ----

// ListKeys is GET /fleet/tenants/{id|me}/keys. Use Me for the calling tenant.
func (c *Client) ListKeys(ctx context.Context, tenant string) ([]APIKey, error) {
	var out items[APIKey]
	err := c.call(ctx, http.MethodGet, "/fleet/tenants/"+seg(tenant)+"/keys", nil, &out)
	return out.Items, err
}

// CreateKey is POST /fleet/tenants/{id|me}/keys (the tenant's primary token, or the operator).
func (c *Client) CreateKey(ctx context.Context, tenant string, req CreateKeyRequest) (*CreatedKey, error) {
	var out CreatedKey
	if err := c.call(ctx, http.MethodPost, "/fleet/tenants/"+seg(tenant)+"/keys", req, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// GetKey finds a key by id or name in ListKeys (central has no single-key route).
func (c *Client) GetKey(ctx context.Context, tenant, idOrName string) (*APIKey, error) {
	keys, err := c.ListKeys(ctx, tenant)
	if err != nil {
		return nil, err
	}
	for i := range keys {
		if keys[i].ID == idOrName {
			return &keys[i], nil
		}
	}
	for i := range keys {
		if keys[i].Name == idOrName {
			return &keys[i], nil
		}
	}
	return nil, &APIError{Status: http.StatusNotFound, Message: fmt.Sprintf("no API key '%s'", idOrName)}
}

// RevokeKey is DELETE /fleet/tenants/{id|me}/keys/{key} (key id or name).
func (c *Client) RevokeKey(ctx context.Context, tenant, idOrName string) (*APIKey, error) {
	var out struct {
		Revoked APIKey `json:"revoked"`
	}
	if err := c.call(ctx, http.MethodDelete, "/fleet/tenants/"+seg(tenant)+"/keys/"+seg(idOrName), nil, &out); err != nil {
		return nil, err
	}
	return &out.Revoked, nil
}

// ---- nodes and capacity ----

// ListNodes is GET /fleet/nodes (operator).
func (c *Client) ListNodes(ctx context.Context) ([]Node, error) {
	var out items[Node]
	err := c.call(ctx, http.MethodGet, "/fleet/nodes", nil, &out)
	return out.Items, err
}

// GetNode is GET /fleet/nodes/{name}: the operator sees everything; a tenant only a Mac it leases.
func (c *Client) GetNode(ctx context.Context, name string) (*Node, error) {
	var out Node
	if err := c.call(ctx, http.MethodGet, "/fleet/nodes/"+seg(name), nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// Capacity is GET /fleet/capacity.
func (c *Client) Capacity(ctx context.Context) (*Capacity, error) {
	var out Capacity
	if err := c.call(ctx, http.MethodGet, "/fleet/capacity", nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// ---- leases ----

// CreateLease is POST /fleet/leases: a whole Mac for at least 24 hours.
func (c *Client) CreateLease(ctx context.Context, req CreateLeaseRequest) (*CreatedLease, error) {
	var out CreatedLease
	if err := c.call(ctx, http.MethodPost, "/fleet/leases", req, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// ListLeases is GET /fleet/leases (a tenant sees its own).
func (c *Client) ListLeases(ctx context.Context, opts ListLeasesOptions) ([]Lease, error) {
	var out items[Lease]
	err := c.call(ctx, http.MethodGet, "/fleet/leases"+query("tenant", opts.Tenant, "state", opts.State), nil, &out)
	return out.Items, err
}

// GetLease is GET /fleet/leases/{id}.
func (c *Client) GetLease(ctx context.Context, id string) (*Lease, error) {
	var out Lease
	if err := c.call(ctx, http.MethodGet, "/fleet/leases/"+seg(id), nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// ReleaseLease is POST /fleet/leases/{id}/release. Refused (409) before the
// 24-hour minimum unless the operator forces it. The tenant's VMs on the Mac
// are deleted and the Mac waits for a scrub.
func (c *Client) ReleaseLease(ctx context.Context, id string, force bool) (*ReleasedLease, error) {
	q := ""
	if force {
		q = "?force=true"
	}
	var out ReleasedLease
	if err := c.call(ctx, http.MethodPost, "/fleet/leases/"+seg(id)+"/release"+q, nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// ---- VMs ----

// LaunchVM is POST /fleet/vms.
func (c *Client) LaunchVM(ctx context.Context, req LaunchRequest) (*LaunchResult, error) {
	var out LaunchResult
	if err := c.call(ctx, http.MethodPost, "/fleet/vms", req, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// ListVMs is GET /fleet/vms (a tenant sees its own).
func (c *Client) ListVMs(ctx context.Context) (*VMList, error) {
	var out VMList
	if err := c.call(ctx, http.MethodGet, "/fleet/vms", nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// GetVM finds one VM in GET /fleet/vms (central has no single-VM route). It
// returns a 404 *APIError when the VM is not listed and its node answered; when
// the node did not answer it returns a 503 *APIError instead, so a caller does
// not mistake an unreachable Mac for a deleted VM.
func (c *Client) GetVM(ctx context.Context, node, id string) (*VM, error) {
	list, err := c.ListVMs(ctx)
	if err != nil {
		return nil, err
	}
	for i := range list.Items {
		if list.Items[i].Node == node && list.Items[i].ID == id {
			return &list.Items[i], nil
		}
	}
	for _, u := range list.UnreachableNodes {
		if u.Node == node {
			return nil, &APIError{Status: http.StatusServiceUnavailable, Message: fmt.Sprintf("node '%s' did not answer: %s", node, u.Reason)}
		}
	}
	return nil, &APIError{Status: http.StatusNotFound, Message: fmt.Sprintf("no VM '%s' on node '%s'", id, node)}
}

// FindVM finds a VM by name in GET /fleet/vms (404 when none, 409 when several share it).
func (c *Client) FindVM(ctx context.Context, name string) (*VM, error) {
	list, err := c.ListVMs(ctx)
	if err != nil {
		return nil, err
	}
	var found *VM
	for i := range list.Items {
		if list.Items[i].Name == name {
			if found != nil {
				return nil, &APIError{Status: http.StatusConflict, Message: fmt.Sprintf("more than one VM is named '%s'", name)}
			}
			found = &list.Items[i]
		}
	}
	if found == nil {
		return nil, &APIError{Status: http.StatusNotFound, Message: fmt.Sprintf("no VM named '%s'", name)}
	}
	return found, nil
}

// DeleteVM is DELETE /fleet/vms/{node}/{id}.
func (c *Client) DeleteVM(ctx context.Context, node, id string) error {
	return c.call(ctx, http.MethodDelete, "/fleet/vms/"+seg(node)+"/"+seg(id), nil, nil)
}

// PowerVM is POST /fleet/vms/{node}/{id}/{action} (start, stop, restart, pause, resume).
// It answers with the node's VM record.
func (c *Client) PowerVM(ctx context.Context, node, id, action string) (*VM, error) {
	switch action {
	case PowerStart, PowerStop, PowerRestart, PowerPause, PowerResume:
	default:
		return nil, fmt.Errorf("fleet: unknown power action %q", action)
	}
	var out VM
	if err := c.call(ctx, http.MethodPost, "/fleet/vms/"+seg(node)+"/"+seg(id)+"/"+action, nil, &out); err != nil {
		return nil, err
	}
	if out.Node == "" {
		out.Node = node
	}
	return &out, nil
}

// Exec is POST /fleet/vms/{node}/{id}/exec: run a command through the guest agent.
func (c *Client) Exec(ctx context.Context, node, id string, req ExecRequest) (*ExecResult, error) {
	var raw json.RawMessage
	if err := c.call(ctx, http.MethodPost, "/fleet/vms/"+seg(node)+"/"+seg(id)+"/exec", req, &raw); err != nil {
		return nil, err
	}
	var out ExecResult
	_ = json.Unmarshal(raw, &out)
	if out.ExitCode == 0 {
		var alt struct {
			ExitStatus *int `json:"exit_status"`
		}
		if json.Unmarshal(raw, &alt) == nil && alt.ExitStatus != nil {
			out.ExitCode = *alt.ExitStatus
		}
	}
	out.Raw = raw
	return &out, nil
}

// VMLogs is GET /fleet/vms/{node}/{id}/logs: the last lines (1-5000, default 200) of the console log.
func (c *Client) VMLogs(ctx context.Context, node, id string, lines int) (string, error) {
	return c.text(ctx, "/fleet/vms/"+seg(node)+"/"+seg(id)+"/logs"+query("lines", itoa(lines)), "text/plain")
}

// ---- networks (overlays) ----

// ListNetworks is GET /fleet/overlays.
func (c *Client) ListNetworks(ctx context.Context) ([]Network, error) {
	var out items[Network]
	err := c.call(ctx, http.MethodGet, "/fleet/overlays", nil, &out)
	return out.Items, err
}

// CreateNetwork is POST /fleet/overlays.
func (c *Client) CreateNetwork(ctx context.Context, req CreateNetworkRequest) (*Network, error) {
	var out Network
	if err := c.call(ctx, http.MethodPost, "/fleet/overlays", req, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// GetNetwork is GET /fleet/overlays/{name}.
func (c *Client) GetNetwork(ctx context.Context, name string) (*Network, error) {
	var out Network
	if err := c.call(ctx, http.MethodGet, "/fleet/overlays/"+seg(name), nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// DeleteNetwork is DELETE /fleet/overlays/{name}.
func (c *Client) DeleteNetwork(ctx context.Context, name string) error {
	return c.call(ctx, http.MethodDelete, "/fleet/overlays/"+seg(name), nil, nil)
}

// AddNetworkMember is POST /fleet/overlays/{name}/members.
func (c *Client) AddNetworkMember(ctx context.Context, name, node string) (*Network, error) {
	var out Network
	if err := c.call(ctx, http.MethodPost, "/fleet/overlays/"+seg(name)+"/members", map[string]string{"node": node}, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// RemoveNetworkMember is DELETE /fleet/overlays/{name}/members/{node}.
func (c *Client) RemoveNetworkMember(ctx context.Context, name, node string) (*Network, error) {
	var out Network
	if err := c.call(ctx, http.MethodDelete, "/fleet/overlays/"+seg(name)+"/members/"+seg(node), nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// RotateNetworkKey is POST /fleet/overlays/{name}/rotate (at most once every 5 minutes).
func (c *Client) RotateNetworkKey(ctx context.Context, name string) (*Network, error) {
	var out Network
	if err := c.call(ctx, http.MethodPost, "/fleet/overlays/"+seg(name)+"/rotate", nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// ---- images ----

// ListImages is GET /fleet/images.
func (c *Client) ListImages(ctx context.Context) ([]Image, error) {
	var out items[Image]
	err := c.call(ctx, http.MethodGet, "/fleet/images", nil, &out)
	return out.Items, err
}

// GetImage finds a catalog entry by name in ListImages.
func (c *Client) GetImage(ctx context.Context, name string) (*Image, error) {
	list, err := c.ListImages(ctx)
	if err != nil {
		return nil, err
	}
	for i := range list {
		if list[i].Name == name {
			return &list[i], nil
		}
	}
	return nil, &APIError{Status: http.StatusNotFound, Message: fmt.Sprintf("no image '%s' in the catalog", name)}
}

// PutImage is PUT /fleet/images/{name} (operator): add or replace a catalog entry.
func (c *Client) PutImage(ctx context.Context, img Image) (*Image, error) {
	var out Image
	if err := c.call(ctx, http.MethodPut, "/fleet/images/"+seg(img.Name), img, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// DeleteImage is DELETE /fleet/images/{name} (operator).
func (c *Client) DeleteImage(ctx context.Context, name string) error {
	return c.call(ctx, http.MethodDelete, "/fleet/images/"+seg(name), nil, nil)
}

// ---- webhooks and notifications ----

// ListWebhooks is GET /fleet/webhooks.
func (c *Client) ListWebhooks(ctx context.Context) (*WebhookList, error) {
	var out WebhookList
	if err := c.call(ctx, http.MethodGet, "/fleet/webhooks", nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// GetWebhook finds a webhook by id in ListWebhooks.
func (c *Client) GetWebhook(ctx context.Context, id string) (*Webhook, error) {
	list, err := c.ListWebhooks(ctx)
	if err != nil {
		return nil, err
	}
	for i := range list.Items {
		if list.Items[i].ID == id {
			return &list.Items[i], nil
		}
	}
	return nil, &APIError{Status: http.StatusNotFound, Message: fmt.Sprintf("no webhook '%s'", id)}
}

// CreateWebhook is POST /fleet/webhooks. Store the secret: it is not shown again.
func (c *Client) CreateWebhook(ctx context.Context, req CreateWebhookRequest) (*CreatedWebhook, error) {
	var out CreatedWebhook
	if err := c.call(ctx, http.MethodPost, "/fleet/webhooks", req, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// DeleteWebhook is DELETE /fleet/webhooks/{id}.
func (c *Client) DeleteWebhook(ctx context.Context, id string) error {
	return c.call(ctx, http.MethodDelete, "/fleet/webhooks/"+seg(id), nil, nil)
}

// TestWebhook is POST /fleet/webhooks/{id}/test: one delivery of a "test" event.
func (c *Client) TestWebhook(ctx context.Context, id string) (*Delivery, error) {
	var out struct {
		Delivery Delivery `json:"delivery"`
	}
	if err := c.call(ctx, http.MethodPost, "/fleet/webhooks/"+seg(id)+"/test", nil, &out); err != nil {
		return nil, err
	}
	return &out.Delivery, nil
}

// Notifications is GET /fleet/notifications, newest first.
func (c *Client) Notifications(ctx context.Context, limit int) ([]Notification, error) {
	var out items[Notification]
	err := c.call(ctx, http.MethodGet, "/fleet/notifications"+query("limit", itoa(limit)), nil, &out)
	return out.Items, err
}

// ---- prices, billing, usage ----

// Prices is GET /fleet/prices.
func (c *Client) Prices(ctx context.Context) (*PriceList, error) {
	var out PriceList
	if err := c.call(ctx, http.MethodGet, "/fleet/prices", nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// SetPrices is PUT /fleet/prices (operator). Running leases keep their price.
func (c *Client) SetPrices(ctx context.Context, p PriceList) (*PriceList, error) {
	var out PriceList
	if err := c.call(ctx, http.MethodPut, "/fleet/prices", p, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// Billing is GET /fleet/billing (a tenant gets its own statement).
func (c *Client) Billing(ctx context.Context, opts BillingOptions) (*Billing, error) {
	var out Billing
	if err := c.call(ctx, http.MethodGet, "/fleet/billing"+query("month", opts.Month, "tenant", opts.Tenant), nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// BillingCSV is GET /fleet/billing?format=csv.
func (c *Client) BillingCSV(ctx context.Context, opts BillingOptions) (string, error) {
	return c.text(ctx, "/fleet/billing"+query("month", opts.Month, "tenant", opts.Tenant, "format", "csv"), "text/csv")
}

func rfc3339(t time.Time) string {
	if t.IsZero() {
		return ""
	}
	return t.UTC().Format(time.RFC3339)
}

// Usage is GET /fleet/usage.
func (c *Client) Usage(ctx context.Context, opts UsageOptions) (*UsageReport, error) {
	var out UsageReport
	q := query("from", rfc3339(opts.From), "to", rfc3339(opts.To), "tenant", opts.Tenant)
	if err := c.call(ctx, http.MethodGet, "/fleet/usage"+q, nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// ---- activity log ----

// Events is GET /fleet/events, newest first (a tenant sees its own actions).
func (c *Client) Events(ctx context.Context, opts EventsOptions) ([]Event, error) {
	var out items[Event]
	err := c.call(ctx, http.MethodGet, "/fleet/events"+query("limit", itoa(opts.Limit), "tenant", opts.Tenant), nil, &out)
	return out.Items, err
}

// ---- policy and reconcile (operator) ----

// Policy is GET /fleet/policy (operator).
func (c *Client) Policy(ctx context.Context) (*Policy, error) {
	var out Policy
	if err := c.call(ctx, http.MethodGet, "/fleet/policy", nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// SetPolicy is PUT /fleet/policy (operator).
func (c *Client) SetPolicy(ctx context.Context, p Policy) (*Policy, error) {
	var out Policy
	if err := c.call(ctx, http.MethodPut, "/fleet/policy", p, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// Reconcile is POST /fleet/reconcile (operator): one reconcile pass now.
func (c *Client) Reconcile(ctx context.Context) (json.RawMessage, error) {
	var out json.RawMessage
	err := c.call(ctx, http.MethodPost, "/fleet/reconcile", nil, &out)
	return out, err
}

// ---- volumes (requires central with volumes) ----

// ListVolumes is GET /fleet/volumes.
func (c *Client) ListVolumes(ctx context.Context) ([]Volume, error) {
	var out items[Volume]
	err := c.call(ctx, http.MethodGet, "/fleet/volumes", nil, &out)
	return out.Items, err
}

// CreateVolume is POST /fleet/volumes.
func (c *Client) CreateVolume(ctx context.Context, req CreateVolumeRequest) (*Volume, error) {
	var out Volume
	if err := c.call(ctx, http.MethodPost, "/fleet/volumes", req, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// GetVolume is GET /fleet/volumes/{name}.
func (c *Client) GetVolume(ctx context.Context, name string) (*Volume, error) {
	var out Volume
	if err := c.call(ctx, http.MethodGet, "/fleet/volumes/"+seg(name), nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// ResizeVolume is POST /fleet/volumes/{name}/resize (grow only).
func (c *Client) ResizeVolume(ctx context.Context, name string, sizeGiB uint64) (*Volume, error) {
	var out Volume
	if err := c.call(ctx, http.MethodPost, "/fleet/volumes/"+seg(name)+"/resize", map[string]uint64{"size_gib": sizeGiB}, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// DeleteVolume is DELETE /fleet/volumes/{name}.
func (c *Client) DeleteVolume(ctx context.Context, name string) error {
	return c.call(ctx, http.MethodDelete, "/fleet/volumes/"+seg(name), nil, nil)
}

// ---- warm pools (requires central with pools) ----

// ListPools is GET /fleet/pools.
func (c *Client) ListPools(ctx context.Context) ([]Pool, error) {
	var out items[Pool]
	err := c.call(ctx, http.MethodGet, "/fleet/pools", nil, &out)
	return out.Items, err
}

// CreatePool is POST /fleet/pools.
func (c *Client) CreatePool(ctx context.Context, req CreatePoolRequest) (*Pool, error) {
	var out Pool
	if err := c.call(ctx, http.MethodPost, "/fleet/pools", req, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// GetPool is GET /fleet/pools/{name}.
func (c *Client) GetPool(ctx context.Context, name string) (*Pool, error) {
	var out Pool
	if err := c.call(ctx, http.MethodGet, "/fleet/pools/"+seg(name), nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// UpdatePool is PATCH /fleet/pools/{name}.
func (c *Client) UpdatePool(ctx context.Context, name string, req UpdatePoolRequest) (*Pool, error) {
	var out Pool
	if err := c.call(ctx, http.MethodPatch, "/fleet/pools/"+seg(name), req, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// DeletePool is DELETE /fleet/pools/{name}.
func (c *Client) DeletePool(ctx context.Context, name string) error {
	return c.call(ctx, http.MethodDelete, "/fleet/pools/"+seg(name), nil, nil)
}
