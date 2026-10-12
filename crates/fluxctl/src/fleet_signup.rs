// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl fleet signup | invoices | invoice-run | payment-method`: self-service accounts and Stripe invoices.

use super::*;

fn money(minor_amount: f64, cur: &str) -> String {
    if cur.eq_ignore_ascii_case("JPY") || cur.eq_ignore_ascii_case("KRW") {
        format!("{minor_amount:.0} {cur}")
    } else {
        format!("{minor_amount:.2} {cur}")
    }
}

/// `fleet signup`: no token; the answer carries the new account's token, once.
pub(super) async fn signup(
    central: &str,
    out: &Out,
    tenant_id: String,
    email: String,
    name: Option<String>,
) -> Result<()> {
    let mut body = json!({ "tenant_id": tenant_id, "email": email });
    if let Some(n) = name {
        body["display_name"] = json!(n);
    }
    let v = call(central, None, Method::POST, "/signup", Some(body))
        .await
        .map_err(|e| match status_of(&e) {
            Some(403) => {
                e.context("sign-up is not open on this central; ask its operator for an account")
            }
            Some(429) => e.context("too many sign-ups; try again in an hour"),
            _ => e,
        })?;
    if !out.human {
        return out.json(&v);
    }
    // Email confirmation (the default): the account is created when the mailed link is confirmed.
    if v.get("email_confirmation").and_then(Value::as_bool) == Some(true) {
        out.done(format!(
            "A confirmation link was sent to {}.",
            out.bold(&email)
        ));
        println!(
            "It is valid for 24 hours. Open it (or run the command below with its token) to create the account"
        );
        println!("and see its access token, once.");
        out.next("fluxctl fleet signup-verify <TOKEN FROM THE LINK>");
        return Ok(());
    }
    print_account(out, central, &tenant_id, &v);
    Ok(())
}

/// `fleet signup-verify TOKEN`: confirms a sign-up from the token in the mailed link and prints the account's token,
/// once. The same thing the link's page does.
pub(super) async fn signup_verify(central: &str, out: &Out, token: String) -> Result<()> {
    let token = token
        .rsplit("token=")
        .next()
        .unwrap_or(&token)
        .trim()
        .to_owned();
    let v = call(
        central,
        None,
        Method::POST,
        "/signup/verify",
        Some(json!({ "token": token })),
    )
    .await
    .map_err(|e| match status_of(&e) {
        Some(400) => e.context("that link has expired or was already used; sign up again"),
        _ => e,
    })?;
    if !out.human {
        return out.json(&v);
    }
    let id = v
        .get("tenant")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    print_account(out, central, &id, &v);
    Ok(())
}

fn print_account(out: &Out, central: &str, tenant_id: &str, v: &Value) {
    let token = s(v, "token");
    out.done(format!("Account {} created.", out.bold(tenant_id)));
    println!();
    kv(out, &[("Token", token.to_owned())]);
    println!();
    println!(
        "{}",
        out.yellow("This is the only time the token is shown. Store it now (a password manager or your CI secrets).")
    );
    println!("Use it with fluxctl:");
    println!("  export CENTRAL_URL={central}");
    println!("  export FLUXVM_AGENT_TOKEN={token}");
    println!("Or sign in to the console at {central}/console");
    if v.get("payment_method_required").and_then(Value::as_bool) == Some(true) {
        println!();
        println!("Add a payment method before you lease a Mac or launch a VM.");
        out.next("fluxctl fleet payment-method");
    } else {
        out.next("fluxctl fleet status");
    }
}

fn status_cell(inv: &Value) -> Cell {
    let failed = inv.get("payment_failed").and_then(Value::as_bool) == Some(true);
    match s(inv, "status") {
        "paid" => ct("Paid", Tone::Ok),
        "open" if failed => ct("Payment failed", Tone::Bad),
        "open" => ct("Due", Tone::Warn),
        "draft" => ct("Draft", Tone::Dim),
        "void" => ct("Void", Tone::Dim),
        "uncollectible" => ct("Uncollectible", Tone::Bad),
        other => ct(other, Tone::Plain),
    }
}

fn amount_of(inv: &Value) -> String {
    money(
        inv.get("amount").and_then(Value::as_f64).unwrap_or(0.0),
        s(inv, "currency"),
    )
}

/// `fleet invoices [--month] [--tenant]`.
pub(super) async fn invoices(
    central: &str,
    token: Option<&str>,
    out: &Out,
    month: Option<String>,
    tenant: Option<String>,
) -> Result<()> {
    let q = query(&[("month", month), ("tenant", tenant)]);
    let v = get(central, token, &format!("/fleet/billing/invoices{q}")).await?;
    if !out.human {
        return out.json(&v);
    }
    let list = items(&v);
    let billing = v.get("billing").cloned().unwrap_or_default();
    let operator = billing.get("mode").is_some();
    if list.is_empty() {
        println!("No invoices yet.");
        if billing.get("payments").and_then(Value::as_bool) == Some(false) {
            println!(
                "{}",
                out.dim("Payments are not set up on this central (fluxvm-agent central --stripe-secret-key-file).")
            );
        } else if operator {
            out.next("fluxctl fleet invoice-run --month <YYYY-MM>");
        }
    } else {
        let mut t = Table::new(if operator {
            vec!["MONTH", "TENANT", "STATUS", "AMOUNT", "PAY"]
        } else {
            vec!["MONTH", "STATUS", "AMOUNT", "PAY"]
        });
        for inv in &list {
            let link = match s(inv, "status") {
                "open" => s(inv, "hosted_invoice_url").to_owned(),
                _ => String::new(),
            };
            let mut row = vec![c(s(inv, "month"))];
            if operator {
                row.push(c(s(inv, "tenant")));
            }
            row.extend([status_cell(inv), c(amount_of(inv)), ct(link, Tone::Dim)]);
            t.row(row);
        }
        t.print(out);
    }
    if billing
        .get("payment_method_pending")
        .and_then(Value::as_bool)
        == Some(true)
    {
        println!();
        println!("Add a payment method before you lease a Mac or launch a VM.");
        out.next("fluxctl fleet payment-method");
    }
    Ok(())
}

/// `fleet invoice-run --month` (operator).
pub(super) async fn invoice_run(
    central: &str,
    token: Option<&str>,
    out: &Out,
    month: String,
    tenant: Option<String>,
    yes: bool,
) -> Result<()> {
    let b = month.as_bytes();
    if b.len() != 7
        || b[4] != b'-'
        || !b
            .iter()
            .enumerate()
            .all(|(i, c)| i == 4 || c.is_ascii_digit())
    {
        bail!("--month looks like 2026-09, not '{month}'");
    }
    let who = tenant
        .as_deref()
        .map_or("every tenant with billing on".to_owned(), |t| {
            format!("tenant {t}")
        });
    if out.human
        && !yes
        && !confirm(&format!(
            "Create Stripe invoices for {month} for {who}? Invoices are sent to customers."
        ))?
    {
        println!("Nothing invoiced.");
        return Ok(());
    }
    let q = query(&[("month", Some(month.clone())), ("tenant", tenant)]);
    let v = call(
        central,
        token,
        Method::POST,
        &format!("/fleet/billing/invoices{q}"),
        None,
    )
    .await?;
    if !out.human {
        return out.json(&v);
    }
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let results = v
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if results.is_empty() {
        println!("No tenant has billing on, so nothing was invoiced.");
        return Ok(());
    }
    let mut t = Table::new(vec!["TENANT", "RESULT", "AMOUNT", "DETAIL"]);
    for r in &results {
        let inv = r.get("invoice").cloned().unwrap_or_default();
        let (result, amount, detail) = match s(r, "outcome") {
            "created" => (
                ct("Invoiced", Tone::Ok),
                amount_of(&inv),
                s(&inv, "hosted_invoice_url").to_owned(),
            ),
            "exists" => (
                ct("Already invoiced", Tone::Dim),
                amount_of(&inv),
                s(&inv, "status").to_owned(),
            ),
            "skipped" => (
                ct("Skipped", Tone::Dim),
                String::new(),
                s(r, "reason").to_owned(),
            ),
            _ => (
                ct("Failed", Tone::Bad),
                String::new(),
                s(r, "reason").to_owned(),
            ),
        };
        t.row(vec![
            c(s(r, "tenant")),
            result,
            c(amount),
            ct(detail, Tone::Dim),
        ]);
    }
    t.print(out);
    println!();
    out.done(format!(
        "{month}: {} invoiced, {} already invoiced, {} with nothing to bill.",
        n("created"),
        n("existing"),
        n("skipped")
    ));
    if n("failed") > 0 {
        bail!(
            "{} invoice(s) failed; run the same command again to retry them (nothing is billed twice)",
            n("failed")
        );
    }
    Ok(())
}

/// `fleet payment-method` (tenant): a Checkout link.
pub(super) async fn payment_method(central: &str, token: Option<&str>, out: &Out) -> Result<()> {
    let v = call(
        central,
        token,
        Method::POST,
        "/fleet/billing/payment-method",
        None,
    )
    .await?;
    if !out.human {
        return out.json(&v);
    }
    println!("Open this link to add a payment method (Stripe):");
    println!("  {}", out.cyan(s(&v, "url")));
    println!(
        "{}",
        out.dim(
            "It brings you back to the console. Leasing and launching work once Stripe confirms it."
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invoice_status_and_amounts_read_well() {
        assert_eq!(status_cell(&json!({"status": "paid"})).text, "Paid");
        assert_eq!(
            status_cell(&json!({"status": "open", "payment_failed": true})).text,
            "Payment failed"
        );
        assert_eq!(status_cell(&json!({"status": "open"})).text, "Due");
        assert_eq!(
            amount_of(&json!({"amount": 43.2, "currency": "USD"})),
            "43.20 USD"
        );
        assert_eq!(
            amount_of(&json!({"amount": 1235.0, "currency": "JPY"})),
            "1235 JPY"
        );
    }
}
