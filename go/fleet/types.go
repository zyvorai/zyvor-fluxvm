// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package fleet

import (
	"encoding/json"
	"time"
)

// Int64 returns a pointer to v (for optional request fields).
func Int64(v int64) *int64 { return &v }

// Int returns a pointer to v.
func Int(v int) *int { return &v }

// Uint64 returns a pointer to v.
func Uint64(v uint64) *uint64 { return &v }

// Uint32 returns a pointer to v.
func Uint32(v uint32) *uint32 { return &v }

// Float64 returns a pointer to v.
func Float64(v float64) *float64 { return &v }

// Bool returns a pointer to v.
func Bool(v bool) *bool { return &v }

// String returns a pointer to v.
func String(v string) *string { return &v }

// ---- tenants and keys ----

// Quota limits what a tenant may use; a nil field is unlimited.
type Quota struct {
	MaxVMs         *uint32 `json:"max_vms,omitempty"`
	MaxVCPUs       *uint64 `json:"max_vcpus,omitempty"`
	MaxMemoryMiB   *uint64 `json:"max_memory_mib,omitempty"`
	MaxLeasedHosts *uint32 `json:"max_leased_hosts,omitempty"`
}

// InUse is what a tenant is using right now.
type InUse struct {
	VMs         uint32 `json:"vms"`
	VCPUs       uint64 `json:"vcpus"`
	MemoryMiB   uint64 `json:"memory_mib"`
	LeasedHosts int    `json:"leased_hosts"`
}

// Tenant is a tenant record as central shows it (never the token hash).
type Tenant struct {
	ID            string    `json:"id"`
	DisplayName   *string   `json:"display_name"`
	Quota         Quota     `json:"quota"`
	Disabled      bool      `json:"disabled"`
	CreatedAt     time.Time `json:"created_at"`
	MonthlyBudget *float64  `json:"monthly_budget"`
	// InUse and ActiveLeases are present on GET /fleet/tenants, /fleet/tenants/{id} and /fleet/tenants/me.
	InUse        *InUse  `json:"in_use,omitempty"`
	ActiveLeases []Lease `json:"active_leases,omitempty"`
}

// CreateTenantRequest is the body of POST /fleet/tenants (operator).
type CreateTenantRequest struct {
	// ID: 1-63 of a-z, 0-9 and '-', starting with a letter or digit; not "me".
	ID          string  `json:"id"`
	DisplayName *string `json:"display_name,omitempty"`
	Quota       Quota   `json:"quota"`
}

// CreatedTenant carries the tenant's primary token, shown only this once.
type CreatedTenant struct {
	Tenant Tenant `json:"tenant"`
	Token  string `json:"token"`
	Note   string `json:"note"`
}

// UpdateTenantRequest is the body of PATCH /fleet/tenants/{id}; nil fields are left alone.
type UpdateTenantRequest struct {
	Quota       *Quota  `json:"quota,omitempty"`
	Disabled    *bool   `json:"disabled,omitempty"`
	DisplayName *string `json:"display_name,omitempty"`
}

// RotatedToken is the answer of POST /fleet/tenants/{id}/rotate-token.
type RotatedToken struct {
	Tenant string `json:"tenant"`
	Token  string `json:"token"`
}

// APIKey is a tenant's named API key (never its hash).
type APIKey struct {
	ID         string     `json:"id"`
	Name       string     `json:"name"`
	CreatedAt  time.Time  `json:"created_at"`
	ExpiresAt  *time.Time `json:"expires_at"`
	ReadOnly   bool       `json:"read_only"`
	LastUsedAt *time.Time `json:"last_used_at"`
}

// CreateKeyRequest is the body of POST /fleet/tenants/{id|me}/keys.
type CreateKeyRequest struct {
	Name string `json:"name"`
	// ExpiresInDays is 1-3650; nil never expires.
	ExpiresInDays *int `json:"expires_in_days,omitempty"`
	ReadOnly      bool `json:"read_only"`
}

// CreatedKey carries the key's token, shown only this once.
type CreatedKey struct {
	Key   APIKey `json:"key"`
	Token string `json:"token"`
	Note  string `json:"note"`
}

// ---- nodes and capacity ----

// Node is a Mac (or Linux host) registered with central. A tenant reading a
// Mac it leases gets only the name, size, free capacity and Apple capabilities.
type Node struct {
	Name           string            `json:"name"`
	FluxvmURL      string            `json:"fluxvm_url,omitempty"`
	VCPUsTotal     uint32            `json:"vcpus_total"`
	MemoryMiBTotal uint64            `json:"memory_mib_total"`
	VMCount        int               `json:"vm_count"`
	LastSeen       time.Time         `json:"last_seen"`
	Healthy        bool              `json:"healthy"`
	Cordoned       bool              `json:"cordoned"`
	Labels         map[string]string `json:"labels,omitempty"`
	FreeVCPUs      uint32            `json:"free_vcpus"`
	FreeMemoryMiB  uint64            `json:"free_memory_mib"`
	Security       json.RawMessage   `json:"security,omitempty"`
	Density        json.RawMessage   `json:"density,omitempty"`
	Apple          *AppleCaps        `json:"apple,omitempty"`
}

// AppleCaps are a Mac's Virtualization.framework capabilities. Raw keeps every
// field the node reported.
type AppleCaps struct {
	NestedVirtualization bool `json:"nested_virtualization"`
	Raw                  map[string]interface{}
}

// UnmarshalJSON keeps the whole object in Raw.
func (a *AppleCaps) UnmarshalJSON(b []byte) error {
	var raw map[string]interface{}
	if err := json.Unmarshal(b, &raw); err != nil {
		return err
	}
	a.Raw = raw
	if v, ok := raw["nested_virtualization"].(bool); ok {
		a.NestedVirtualization = v
	}
	return nil
}

// MarshalJSON writes Raw back.
func (a AppleCaps) MarshalJSON() ([]byte, error) {
	if a.Raw != nil {
		return json.Marshal(a.Raw)
	}
	return json.Marshal(map[string]interface{}{"nested_virtualization": a.NestedVirtualization})
}

// Capacity is GET /fleet/capacity, from the nodes' last heartbeats.
type Capacity struct {
	NodesTotal       int    `json:"nodes_total"`
	NodesHealthy     int    `json:"nodes_healthy"`
	NodesCordoned    int    `json:"nodes_cordoned"`
	NodesSchedulable int    `json:"nodes_schedulable"`
	VMCount          int    `json:"vm_count"`
	VCPUsTotal       uint64 `json:"vcpus_total"`
	VCPUsUsed        uint64 `json:"vcpus_used"`
	VCPUsFree        uint64 `json:"vcpus_free"`
	MemoryMiBTotal   uint64 `json:"memory_mib_total"`
	MemoryMiBUsed    uint64 `json:"memory_mib_used"`
	MemoryMiBFree    uint64 `json:"memory_mib_free"`
}

// ---- leases ----

// Lease states.
const (
	LeaseActive   = "active"
	LeaseReleased = "released"
)

// Lease is a whole Mac rented by one tenant for at least 24 hours.
type Lease struct {
	ID         string     `json:"id"`
	Tenant     string     `json:"tenant"`
	Node       string     `json:"node"`
	StartedAt  time.Time  `json:"started_at"`
	MinEnd     time.Time  `json:"min_end"`
	EndsAt     *time.Time `json:"ends_at,omitempty"`
	ReleasedAt *time.Time `json:"released_at,omitempty"`
	State      string     `json:"state"`
	RateMicros *int64     `json:"rate_micros,omitempty"`
	PriceClass string     `json:"price_class,omitempty"`
}

// CreateLeaseRequest is the body of POST /fleet/leases.
type CreateLeaseRequest struct {
	// Tenant is required for the operator (a tenant always leases for itself).
	Tenant       string            `json:"tenant,omitempty"`
	Node         string            `json:"node,omitempty"`
	NodeSelector map[string]string `json:"nodeSelector,omitempty"`
	// Hours sets an end time (at least 24); nil runs until released.
	Hours                *int64 `json:"hours,omitempty"`
	MinVCPUs             uint32 `json:"min_vcpus,omitempty"`
	MinMemoryMiB         uint64 `json:"min_memory_mib,omitempty"`
	NestedVirtualization bool   `json:"nested_virtualization,omitempty"`
	// AnyNode and AllowBusy are operator-only.
	AnyNode   bool `json:"any_node,omitempty"`
	AllowBusy bool `json:"allow_busy,omitempty"`
}

// CreatedLease is the answer of POST /fleet/leases.
type CreatedLease struct {
	Lease Lease `json:"lease"`
	Node  Node  `json:"node"`
}

// ListLeasesOptions filters GET /fleet/leases.
type ListLeasesOptions struct {
	Tenant string // operator only
	State  string // "active" or "released"
}

// ReleasedLease is the answer of POST /fleet/leases/{id}/release.
type ReleasedLease struct {
	Lease         Lease             `json:"lease"`
	Evicted       []json.RawMessage `json:"evicted"`
	ScrubRequired bool              `json:"scrub_required"`
}

// ---- VMs ----

// VM is a VM record as the node reports it, plus the node it runs on. Request
// is the create request the node stored (central stamps "tenant" into it).
type VM struct {
	ID        string                 `json:"id"`
	Name      string                 `json:"name"`
	Node      string                 `json:"node"`
	Backend   string                 `json:"backend"`
	Status    string                 `json:"status"`
	CreatedAt time.Time              `json:"created_at"`
	ExpiresAt *time.Time             `json:"expires_at"`
	Error     *string                `json:"error"`
	Request   map[string]interface{} `json:"request,omitempty"`
}

// VCPUs is the requested vCPU count, from Request.
func (v VM) VCPUs() int64 { return reqInt(v.Request, "vcpus") }

// MemoryMiB is the requested memory, from Request.
func (v VM) MemoryMiB() int64 { return reqInt(v.Request, "memory_mib") }

// Image is the disk path the VM was created from, from Request.
func (v VM) Image() string {
	s, _ := v.Request["image"].(string)
	return s
}

// GuestOS is "macos" or "linux" for a vz VM, from Request.apple.guest_os ("" otherwise).
func (v VM) GuestOS() string {
	if a, ok := v.Request["apple"].(map[string]interface{}); ok {
		s, _ := a["guest_os"].(string)
		return s
	}
	return ""
}

func reqInt(m map[string]interface{}, k string) int64 {
	if f, ok := m[k].(float64); ok {
		return int64(f)
	}
	return 0
}

// NetworkRef joins a VM to a private network (overlay) by name.
type NetworkRef struct {
	Name string `json:"name"`
}

// AppleOptions are the Virtualization.framework options of a launch.
type AppleOptions struct {
	// GuestOS is "macos" or "linux".
	GuestOS  string       `json:"guest_os"`
	Networks []NetworkRef `json:"networks,omitempty"`
}

// LaunchRequest is the body of POST /fleet/vms: a node's CreateVmRequest plus
// central's placement fields (node, nodeSelector, reschedulable, tenant).
// Extra is merged into the JSON for request fields this struct does not name
// (cloud_init, agent, labels, ...); Extra never overrides a named field.
type LaunchRequest struct {
	Name string `json:"name"`
	// Backend defaults to "vz".
	Backend     string        `json:"backend"`
	Image       string        `json:"image"`
	VCPUs       int           `json:"vcpus,omitempty"`
	MemoryMiB   uint64        `json:"memory_mib,omitempty"`
	DiskSizeGiB *uint64       `json:"disk_size_gib,omitempty"`
	TTLSeconds  *uint64       `json:"ttl_seconds,omitempty"`
	Apple       *AppleOptions `json:"apple,omitempty"`
	// Node pins the VM to one Mac; otherwise central places it.
	Node         string            `json:"node,omitempty"`
	NodeSelector map[string]string `json:"nodeSelector,omitempty"`
	// Reschedulable keeps the spec so a drain can recreate the VM elsewhere. Leave it off for bodies with secrets.
	Reschedulable bool `json:"reschedulable,omitempty"`
	// Tenant lets the operator act for a tenant (ignored for tenant tokens).
	Tenant string `json:"tenant,omitempty"`
	// Volumes attaches block volumes by name (requires central with volumes).
	Volumes []string               `json:"volumes,omitempty"`
	Extra   map[string]interface{} `json:"-"`
}

// MarshalJSON merges Extra under the named fields.
func (r LaunchRequest) MarshalJSON() ([]byte, error) {
	type plain LaunchRequest
	p := plain(r)
	if p.Backend == "" {
		p.Backend = "vz"
	}
	b, err := json.Marshal(p)
	if err != nil || len(r.Extra) == 0 {
		return b, err
	}
	var m map[string]interface{}
	if err := json.Unmarshal(b, &m); err != nil {
		return nil, err
	}
	for k, v := range r.Extra {
		if _, set := m[k]; !set {
			m[k] = v
		}
	}
	return json.Marshal(m)
}

// LaunchResult is the answer of POST /fleet/vms.
type LaunchResult struct {
	Node string `json:"node"`
	VM   VM     `json:"vm"`
}

// Unreachable names a node GET /fleet/vms could not account for.
type Unreachable struct {
	Node   string `json:"node"`
	Reason string `json:"reason"`
}

// VMList is the answer of GET /fleet/vms. Check UnreachableNodes before
// concluding a VM is gone.
type VMList struct {
	Items            []VM          `json:"items"`
	UnreachableNodes []Unreachable `json:"unreachable_nodes"`
}

// ExecRequest is the body of POST /fleet/vms/{node}/{id}/exec.
type ExecRequest struct {
	Command        string `json:"command"`
	TimeoutSeconds *int   `json:"timeout_seconds,omitempty"`
}

// ExecResult is the guest agent's answer. Raw keeps every field.
type ExecResult struct {
	ExitCode int    `json:"exit_code"`
	Stdout   string `json:"stdout"`
	Stderr   string `json:"stderr"`
	Raw      json.RawMessage
}

// Power actions for PowerVM.
const (
	PowerStart   = "start"
	PowerStop    = "stop"
	PowerRestart = "restart"
	PowerPause   = "pause"
	PowerResume  = "resume"
)

// ---- networks (overlays) ----

// Network is a private network spanning Macs. Members maps each member Mac to
// its slice of host numbers (inclusive). The key is never shown.
type Network struct {
	Name         string            `json:"name"`
	Tenant       *string           `json:"tenant"`
	Subnet       string            `json:"subnet"`
	Port         int               `json:"port"`
	MTU          int               `json:"mtu"`
	Members      map[string][2]int `json:"members"`
	KeyEpoch     int               `json:"key_epoch"`
	KeyRotatedAt time.Time         `json:"key_rotated_at"`
	CreatedAt    time.Time         `json:"created_at"`
}

// CreateNetworkRequest is the body of POST /fleet/overlays.
type CreateNetworkRequest struct {
	// Name: 1-40 of a-z, 0-9 and '-', not starting or ending with '-'.
	Name string `json:"name"`
	// Tenant: operator only.
	Tenant string `json:"tenant,omitempty"`
	// Nodes are the member Macs; nil means every Mac the tenant leases.
	Nodes        []string `json:"nodes,omitempty"`
	HostsPerNode int      `json:"hosts_per_node,omitempty"`
	MTU          int      `json:"mtu,omitempty"`
}

// ---- images ----

// Image is an image catalog entry.
type Image struct {
	Name        string     `json:"name"`
	DisplayName *string    `json:"display_name"`
	Description *string    `json:"description"`
	GuestOS     string     `json:"guest_os"`
	Image       string     `json:"image"`
	VCPUs       int        `json:"vcpus"`
	MemoryMiB   uint64     `json:"memory_mib"`
	DiskSizeGiB *uint64    `json:"disk_size_gib"`
	Source      *string    `json:"source"`
	UpdatedAt   *time.Time `json:"updated_at"`
}

// ---- webhooks and notifications ----

// Delivery is one attempt to deliver a notification to a webhook.
type Delivery struct {
	Notification string    `json:"notification"`
	Type         string    `json:"type"`
	Time         time.Time `json:"time"`
	Attempt      int       `json:"attempt"`
	Status       *int      `json:"status"`
	OK           bool      `json:"ok"`
	Error        string    `json:"error,omitempty"`
}

// Webhook receives notifications as signed JSON (never its secret).
type Webhook struct {
	ID         string     `json:"id"`
	Owner      *string    `json:"owner"`
	URL        string     `json:"url"`
	Events     []string   `json:"events"`
	CreatedAt  time.Time  `json:"created_at"`
	Deliveries []Delivery `json:"deliveries,omitempty"`
}

// WebhookList is GET /fleet/webhooks: the webhooks and the event kinds the caller may subscribe to.
type WebhookList struct {
	Items []Webhook `json:"items"`
	Kinds []string  `json:"kinds"`
}

// CreateWebhookRequest is the body of POST /fleet/webhooks. Empty Events sends every event.
type CreateWebhookRequest struct {
	URL    string   `json:"url"`
	Events []string `json:"events,omitempty"`
}

// CreatedWebhook carries the signing secret, shown only this once.
type CreatedWebhook struct {
	Webhook Webhook `json:"webhook"`
	Secret  string  `json:"secret"`
	Note    string  `json:"note"`
}

// Notification is one event central told someone about.
type Notification struct {
	ID     string    `json:"id"`
	Time   time.Time `json:"time"`
	Type   string    `json:"type"`
	Tenant string    `json:"tenant,omitempty"`
	Title  string    `json:"title"`
	Detail string    `json:"detail,omitempty"`
	Link   string    `json:"link,omitempty"`
}

// ---- prices, billing, budgets, usage ----

// MacClass is a size class of Mac and its hourly lease price.
type MacClass struct {
	Name         string  `json:"name"`
	MinVCPUs     uint32  `json:"min_vcpus"`
	MinMemoryGiB uint64  `json:"min_memory_gib"`
	PerHour      float64 `json:"per_hour"`
}

// PriceList is GET/PUT /fleet/prices.
type PriceList struct {
	Currency              string     `json:"currency"`
	MacClasses            []MacClass `json:"mac_classes"`
	MacPerHour            float64    `json:"mac_per_hour"`
	VMPerHour             float64    `json:"vm_per_hour"`
	VCPUPerHour           float64    `json:"vcpu_per_hour"`
	MemoryGiBPerHour      float64    `json:"memory_gib_per_hour"`
	MacOSVMPerHour        float64    `json:"macos_vm_per_hour"`
	ChargeVMsOnLeasedMacs bool       `json:"charge_vms_on_leased_macs"`
}

// Line is one line of a statement.
type Line struct {
	Description string  `json:"description"`
	Quantity    float64 `json:"quantity"`
	Unit        string  `json:"unit"`
	UnitPrice   float64 `json:"unit_price"`
	AmountMicro int64   `json:"amount_micros"`
	Amount      float64 `json:"amount"`
}

// Budget is a tenant's budget state for the month.
type Budget struct {
	Monthly float64 `json:"monthly"`
	Used    float64 `json:"used"`
	// State is "ok", "near" (80% or more) or "over".
	State string `json:"state"`
}

// Statement is one tenant's bill for a month.
type Statement struct {
	Tenant      string    `json:"tenant"`
	Currency    string    `json:"currency"`
	From        time.Time `json:"from"`
	To          time.Time `json:"to"`
	Lines       []Line    `json:"lines"`
	TotalMicros int64     `json:"total_micros"`
	Total       float64   `json:"total"`
	Forecast    *float64  `json:"forecast,omitempty"`
	Budget      *Budget   `json:"budget,omitempty"`
}

// Billing is GET /fleet/billing.
type Billing struct {
	From       time.Time   `json:"from"`
	To         time.Time   `json:"to"`
	Currency   string      `json:"currency"`
	PricesSet  bool        `json:"prices_set"`
	Statements []Statement `json:"statements"`
}

// BillingOptions selects a month ("2026-10"; default this month) and, for the operator, one tenant.
type BillingOptions struct {
	Month  string
	Tenant string
}

// TenantUsage is one tenant's metered use in a window.
type TenantUsage struct {
	Tenant               string `json:"tenant"`
	VMSeconds            int64  `json:"vm_seconds"`
	VCPUSeconds          int64  `json:"vcpu_seconds"`
	MemoryMiBSeconds     int64  `json:"memory_mib_seconds"`
	MacOSVMSeconds       int64  `json:"macos_vm_seconds"`
	LeaseSeconds         int64  `json:"lease_seconds"`
	BillableLeaseSeconds int64  `json:"billable_lease_seconds"`
	VMsStarted           uint32 `json:"vms_started"`
	LeasesStarted        uint32 `json:"leases_started"`
}

// UsageReport is GET /fleet/usage.
type UsageReport struct {
	From    time.Time     `json:"from"`
	To      time.Time     `json:"to"`
	Tenants []TenantUsage `json:"tenants"`
}

// UsageOptions selects the window (default the last 30 days) and, for the operator, one tenant.
type UsageOptions struct {
	From, To time.Time
	Tenant   string
}

// ---- activity log ----

// Event is one entry of the activity log.
type Event struct {
	Time   time.Time `json:"time"`
	Actor  string    `json:"actor"`
	Tenant string    `json:"tenant,omitempty"`
	Method string    `json:"method"`
	Path   string    `json:"path"`
	Target string    `json:"target,omitempty"`
	Status int       `json:"status"`
	Key    string    `json:"key,omitempty"`
}

// EventsOptions filters GET /fleet/events.
type EventsOptions struct {
	Limit  int
	Tenant string // operator only
}

// ---- policy ----

// Policy is the fleet-wide rules (GET/PUT /fleet/policy, operator).
type Policy struct {
	RequireLeaseForMacOS   bool   `json:"require_lease_for_macos"`
	ExclusiveLeasedHosts   bool   `json:"exclusive_leased_hosts"`
	MinLeaseHours          int64  `json:"min_lease_hours"`
	LedgerRetentionDays    int64  `json:"ledger_retention_days"`
	IsolateTenantNetworks  bool   `json:"isolate_tenant_networks"`
	TenantSubnetPrefix     string `json:"tenant_subnet_prefix"`
	OverlayKeyRotationDays int64  `json:"overlay_key_rotation_days"`
}

// ---- volumes (requires central with volumes) ----

// Volume is a block volume on one Mac.
type Volume struct {
	ID         string    `json:"id"`
	Name       string    `json:"name"`
	Tenant     *string   `json:"tenant"`
	Node       string    `json:"node"`
	SizeGiB    uint64    `json:"size_gib"`
	AttachedTo *string   `json:"attached_to"`
	CreatedAt  time.Time `json:"created_at"`
}

// CreateVolumeRequest is the body of POST /fleet/volumes.
type CreateVolumeRequest struct {
	Name    string `json:"name"`
	SizeGiB uint64 `json:"size_gib"`
	// Node pins the volume to one Mac; otherwise central picks one.
	Node string `json:"node,omitempty"`
}

// ---- warm pools (requires central with pools) ----

// Pool keeps VMs booted from one image ready to hand out.
//
// Central reports the size as `size: {vcpus, memory_mib}` and the counts under `status`; VCPUs, MemoryMiB, Ready and
// Total are filled from them (see UnmarshalJSON), so callers need not dig.
type Pool struct {
	Name   string    `json:"name"`
	Owner  string    `json:"owner,omitempty"`
	Tenant *string   `json:"tenant,omitempty"`
	Image  string    `json:"image"`
	Size   *PoolSize `json:"size,omitempty"`
	// VCPUs and MemoryMiB are Size's, for convenience.
	VCPUs        *int              `json:"vcpus,omitempty"`
	MemoryMiB    *uint64           `json:"memory_mib,omitempty"`
	MinReady     int               `json:"min_ready"`
	MaxTotal     *int              `json:"max_total,omitempty"`
	NodeSelector map[string]string `json:"node_selector,omitempty"`
	Nodes        []string          `json:"nodes,omitempty"`
	Status       *PoolStatus       `json:"status,omitempty"`
	// Ready is Status.Ready; Total is every VM of the pool alive: ready, booting and claimed ones still running.
	Ready     int        `json:"ready,omitempty"`
	Total     int        `json:"total,omitempty"`
	CreatedAt *time.Time `json:"created_at,omitempty"`
}

// PoolSize is a pool's VM size.
type PoolSize struct {
	VCPUs     int    `json:"vcpus"`
	MemoryMiB uint64 `json:"memory_mib"`
}

// PoolStatus is what central reports about a pool right now.
type PoolStatus struct {
	State          string     `json:"state"`
	Ready          int        `json:"ready"`
	Booting        int        `json:"booting"`
	ClaimedRunning int        `json:"claimed_running"`
	Claimed24h     int        `json:"claimed_24h"`
	AvgClaimMs     *float64   `json:"avg_claim_ms,omitempty"`
	LastClaimAt    *time.Time `json:"last_claim_at,omitempty"`
	LastError      *string    `json:"last_error,omitempty"`
}

// UnmarshalJSON reads central's pool and fills the convenience fields from `size` and `status`.
func (p *Pool) UnmarshalJSON(b []byte) error {
	type plain Pool
	var raw plain
	if err := json.Unmarshal(b, &raw); err != nil {
		return err
	}
	*p = Pool(raw)
	if p.Size != nil {
		v, m := p.Size.VCPUs, p.Size.MemoryMiB
		p.VCPUs, p.MemoryMiB = &v, &m
	}
	if p.Status != nil {
		p.Ready = p.Status.Ready
		p.Total = p.Status.Ready + p.Status.Booting + p.Status.ClaimedRunning
	}
	return nil
}

// CreatePoolRequest is the body of POST /fleet/pools.
type CreatePoolRequest struct {
	Name      string   `json:"name"`
	Image     string   `json:"image"`
	VCPUs     *int     `json:"vcpus,omitempty"`
	MemoryMiB *uint64  `json:"memory_mib,omitempty"`
	MinReady  int      `json:"min_ready"`
	MaxTotal  *int     `json:"max_total,omitempty"`
	Nodes     []string `json:"nodes,omitempty"`
}

// UpdatePoolRequest is the body of PATCH /fleet/pools/{name}; nil fields are left alone.
type UpdatePoolRequest struct {
	Image     *string  `json:"image,omitempty"`
	VCPUs     *int     `json:"vcpus,omitempty"`
	MemoryMiB *uint64  `json:"memory_mib,omitempty"`
	MinReady  *int     `json:"min_ready,omitempty"`
	MaxTotal  *int     `json:"max_total,omitempty"`
	Nodes     []string `json:"nodes,omitempty"`
}
