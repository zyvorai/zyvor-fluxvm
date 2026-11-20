# Mac cloud: self-service sign-up and Stripe payments

Central can let people create their own account (a tenant) from the console's sign-in page or with `fluxctl`, and can
turn each tenant's monthly statement (see [mac-cloud-tenancy.md](mac-cloud-tenancy.md#prices-and-billing)) into a
Stripe invoice. Both are off until the operator turns them on.

- **Sign-up**: `POST /signup` creates a tenant with a default quota and answers with its token once, like
  `POST /fleet/tenants`. It is rate limited, can be restricted to email domains, and can require a saved payment
  method before the new tenant leases a Mac or launches a VM.
- **Invoices**: once a month (by hand or on the 1st), each tenant with invoicing on gets a Stripe invoice with a line
  per statement line. Stripe charges a saved card, or emails a link to pay. Running it twice never bills twice.
- **Payment methods**: a tenant saves a card through Stripe Checkout (setup mode). Card details go to Stripe only.
- **Webhooks**: Stripe tells central when an invoice is paid or a payment fails, and when a card was saved.

## Setting up Stripe

1. **Keys.** In the Stripe dashboard (Developers, API keys) create a restricted key with write access to Customers,
   Invoices, Invoice items, Checkout Sessions and Setup Intents (read is enough for Setup Intents), or use the secret
   key. Use a test-mode key (`sk_test_…` / `rk_test_…`) first. Put it in a file only central's user can read:

   ```bash
   install -m 600 /dev/null /etc/fluxvm/stripe-key && printf '%s' 'rk_live_…' > /etc/fluxvm/stripe-key
   ```

2. **Webhook endpoint.** In Developers, Webhooks, add an endpoint at `https://<central>/stripe/webhook` with these
   events:

   | Event | What central does |
   |---|---|
   | `invoice.paid`, `invoice.payment_succeeded` | marks the invoice paid; re-enables a tenant suspended for it |
   | `invoice.payment_failed` | marks the payment failed and notifies the tenant (`billing.payment_failed`) |
   | `invoice.finalized`, `invoice.updated`, `invoice.voided`, `invoice.marked_uncollectible` | keeps the status in step |
   | `checkout.session.completed` | saves the card as the customer's default for invoices; lets the tenant lease |

   Copy the endpoint's signing secret (`whsec_…`) into another file (`/etc/fluxvm/stripe-webhook-secret`, mode 600).
   The endpoint must reach central over HTTPS; behind a load balancer, pass the request body through unchanged (the
   signature covers the exact bytes).

3. **Start central** with both, and the URL people use for the console (Checkout sends them back there):

   ```bash
   fluxvm-agent central --token "$FLEET_TOKEN" \
     --stripe-secret-key-file /etc/fluxvm/stripe-key \
     --stripe-webhook-secret-file /etc/fluxvm/stripe-webhook-secret \
     --public-url https://cloud.example.com
   ```

   `STRIPE_SECRET_KEY` and `STRIPE_WEBHOOK_SECRET` can hold the secrets themselves instead of the files (handy with a
   secret manager that sets environment variables). The key is sent only to Stripe in the `Authorization` header: it
   is never logged, written to `fleet-tenancy.json` or returned by the API. `--stripe-api-base` points central at a
   mock for testing. Without `--public-url`, Checkout's return links use the request's `Host` header.

4. **Prices.** Invoices are made from the statements, so set a price list first (`fluxctl fleet prices --set`). The
   price list's currency is the invoice currency.

Testing locally: `stripe listen --forward-to localhost:7799/stripe/webhook` prints a `whsec_…` for the forwarded
events; use it as the webhook secret, and a test-mode key.

## Turning on sign-up

In the console, Policy, Sign-up (or `PUT /fleet/policy`):

| Policy field | Default | Meaning |
|---|---|---|
| `signup_enabled` | `false` | Anyone can create an account; a "Create an account" link appears on the sign-in page |
| `signup_require_email_verification` | `true` | The account is created only when the person opens a link mailed to their address (see [Email confirmation](#email-confirmation)). `false` is for labs: anyone can then sign up as anyone |
| `signup_default_quota` | 1 Mac, 2 VMs, 8 vCPUs, 16 GiB | The quota a signed-up tenant starts with |
| `signup_require_payment_method` | `false` | A new tenant saves a card before it may `POST /fleet/leases` or `POST /fleet/vms` (`402` until then) |
| `signup_allowed_email_domains` | any | Only these domains (exact match, e.g. `["acme.io"]`) |
| `billing_auto_invoice` | `false` | On the 1st of each month, invoice last month |
| `billing_suspend_unpaid` | `false` | Disable a tenant whose invoice is still unpaid `billing_suspend_after_days` after it was sent |
| `billing_suspend_after_days` | `14` | |

Sign-up stays closed, whatever the policy, when central runs without `--token` (tenants need it), when a payment
method is required but Stripe is not set up, and when email confirmation is required but central has no mail webhook.
`GET /signup/config` tells the console which it is (`open`, `email_confirmation`, `payments`).

What a sign-up does:

- Checks the account ID like `POST /fleet/tenants` (1-63 of `a-z`, `0-9`, `-`; a few names such as `admin` are
  reserved), the email's shape and domain, and that no tenant already has that email.
- Limits sign-ups to 5 an hour per client address (IPv6 per /64; behind a reverse proxy on the same machine, the last
  `X-Forwarded-For` entry) and 100 an hour in all. Invalid requests are refused before they count.
- With email confirmation (the default) it records a **pending** sign-up and mails a link; nothing else exists yet
  (no tenant, no token, no Stripe customer). Answers `202`.
- Creates the tenant (at confirmation, or at once when confirmation is off) with `signup_default_quota`, stores the
  email as `owner_email` (the SSO feature makes that person the owner), turns invoicing on when Stripe is set up, and
  creates the Stripe customer (if Stripe is down, it is created later, with the first invoice or card).
- Answers `201` with the token, once, from the confirmation (or from `POST /signup` when confirmation is off). The
  console shows it on a screen that cannot be dismissed until the person says they stored it, with Copy and Download,
  then signs them in.

```bash
fluxctl fleet --central https://cloud.example.com signup acme --email ops@acme.io --name "Acme Mobile"
# ...open the link in the mail, or:
fluxctl fleet --central https://cloud.example.com signup-verify fvv_...
fluxctl fleet payment-method        # with the new token: a Stripe Checkout link
```

## Email confirmation

An address nobody confirmed proves nothing: anyone could sign up as someone else and receive the invoices (and, through
Stripe, mail from your brand) meant for them. So by default a sign-up is a request, and the account exists only after
the person opens the link mailed to that address.

1. `POST /signup` validates the request, records it as pending (24 hours; at most 2000 at once) and answers `202`. The
   answer is the same whether or not the address already has an account; an address that does gets a different mail
   ("you already have an account") so the form cannot be used to find out who is registered. A tenant ID that is taken
   is said plainly (an ID is not a secret). A second request for one address within 5 minutes sends nothing more.
2. Central POSTs the mail to **your mail webhook** (below). Central does not speak SMTP.
3. The link (`{--public-url}/signup/verify?token=fvv_…`) opens a page that shows the account and an
   **Confirm** button. Only the button's `POST` creates the account, so a mail scanner that fetches every link does not
   use it up. The token is stored only as its SHA-256 and works once.
4. Confirming creates the tenant with `owner_email_verified` set (so the first SSO sign-in of that person does **not**
   rotate the token, unlike an unconfirmed sign-up) and shows the access token once. The same step as JSON:
   `POST /signup/verify {"token": "fvv_…"}` (`fluxctl fleet signup-verify`).

### The mail webhook

```bash
fluxvm-agent central --token "$TOKEN" --public-url https://cloud.example.com \
  --signup-mail-webhook https://mailer.internal/send --signup-mail-secret-file /etc/fluxvm/mail-secret
```

Central sends `POST` with `Content-Type: application/json`, and `X-FluxVM-Signature: sha256=<hex>` (HMAC-SHA256 of the
body with the secret file's contents) when a secret is set:

```json
{"type": "signup.verify", "to": "ops@acme.io", "tenant_id": "acme",
 "link": "https://cloud.example.com/signup/verify?token=fvv_…", "expires_in_hours": 24,
 "subject": "Confirm your FluxVM Cloud account", "text": "…the message, with the link…"}
```

`type` is `signup.verify` or `signup.exists` (no link; a `console` URL instead). `subject` and `text` are ready to send,
so the receiver can be a few lines in front of SES, Postmark, Mailgun or an SMTP relay: verify the signature, then send
`text` to `to` with `subject`. A non-2xx answer, or no answer in 10 seconds, is logged and not retried; the person is
told the same thing either way and can ask again after 5 minutes.

`--signup-mail-webhook log` prints the mail, with the link, on central's standard error: for trying it out, never
for real users. Plain `http://` is accepted only with a warning (the links cross the network unencrypted).

Turning `signup_require_email_verification` off restores the earlier behaviour (`201` with the token at once, address
unconfirmed): for a closed lab only.

## Invoices

```bash
fluxctl fleet invoice-run --month 2026-09     # operator; asks first on a terminal (-y to skip)
fluxctl fleet invoices [--month 2026-09]      # a tenant's own, or everyone's for the operator
```

In the console, the operator's Billing page has "Create invoices for <month>" (with a confirmation) and each billed
tenant's invoice status for that month; a tenant's Billing page lists its invoices with a Pay link.

For each tenant with invoicing on, an invoice run for a finished month:

1. Makes the tenant's statement for the month. Each line with an amount becomes an invoice item, converted from
   central's micro-units to the currency's smallest unit (cents for USD; whole yen for JPY and Stripe's other
   zero-decimal currencies; multiples of 10 for three-decimal currencies), rounding half away from zero. A month with
   nothing to bill is skipped.
2. Creates a draft invoice (`pending_invoice_items_behavior=exclude`, so nothing else on the customer slips in),
   charged automatically when the tenant has saved a card, otherwise sent with 30 days to pay.
3. Adds the items, then finalizes it, and stores the invoice id, number, status, total and pay link
   (`fleet-tenancy.json`, `invoices`).

Every Stripe call carries an `Idempotency-Key` made of the tenant, its creation time, the month and the step
(`fluxvm-invoice-acme-1759276800-2026-09-item-0`), and central remembers each tenant's invoice for each month: running
the same month again skips tenants already invoiced, and a run that stopped half-way (Stripe down, central restarted)
continues from the step where it stopped without adding lines twice. The current month cannot be invoiced.

`PUT /fleet/tenants/{id}/billing {"enabled": true}` turns invoicing on for a tenant the operator created (and creates
its Stripe customer); `{"payment_method_required": false}` waives the card for a signed-up tenant. The tenant's
details in the console show its owner email, invoicing state, payment method and a link to the customer in the Stripe
dashboard.

## Unpaid invoices

A failed payment marks the invoice "Payment failed" and sends the tenant a `billing.payment_failed` notification (to
its webhooks too). With `billing_suspend_unpaid` on, a tenant whose invoice is still unpaid
`billing_suspend_after_days` after it was sent is disabled (`billing.suspended`); it is enabled again as soon as Stripe
reports the invoice paid. Tenants the operator disabled by hand are never re-enabled by a payment.

## API

| Route | Who |
|---|---|
| `GET /signup/config` | anyone: `{open, require_payment_method, payments, allowed_email_domains}` |
| `POST /signup` (`{tenant_id, display_name, email}`) | anyone, when open |
| `POST /stripe/webhook` | Stripe, signed (`Stripe-Signature: t=…,v1=…`, HMAC-SHA256 of `t.body`, 5 minutes tolerance) |
| `GET /fleet/billing/invoices[?month=&tenant=]` | tenant (own), operator |
| `POST /fleet/billing/invoices?month=YYYY-MM[&tenant=T]` | operator |
| `POST /fleet/billing/payment-method` | tenant (primary token or a full key): `{url}` of a Checkout session in setup mode |
| `PUT /fleet/tenants/{id}/billing` (`{enabled, payment_method_required}`) | operator |

Tenant records gain `owner_email`, `stripe_customer_id`, `billing_enabled`, `has_payment_method`,
`payment_method_pending`, `self_service` and `suspended_for_nonpayment`; older state files load with them empty.

## Not done yet

- Taxes, coupons and credit notes: do them in Stripe on the invoice.
- Refunds and disputes are handled in the Stripe dashboard; central only follows the invoice status.
- The webhook-event dedup list and the sign-up rate limits are in memory, per central.
- **Email confirmation depends on your mail path.** Central hands the message to your webhook and does not retry or
  see delivery: a lost mail means the person asks again. Pending sign-ups and their rate limits live in central's
  state and memory (pending ones survive a restart; the rate limits do not).
- With confirmation turned off (`signup_require_email_verification: false`) the address is unconfirmed: it is shown as
  the owner, Stripe's invoice emails go to it, and the first SSO sign-in with it takes the tenant over and rotates the
  sign-up token. Keep that to labs.
- Payment gating covers creating VMs, leases, clones, volumes (and growing one), warm pools (and resizing one), and
  starting, restarting, resuming and snapshot restore, for a tenant without a payment method.
- A sign-up is limited to 5 per client address and 100 in total per hour. One caller spread over many addresses can use up
  the 100 and lock real sign-ups out for that hour.
