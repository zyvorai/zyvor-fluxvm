// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Predictive warm pool helpers.

use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct Predictor {
    counts: HashMap<String, u64>, // key = "image@size"
}

impl Predictor {
    pub fn observe(&mut self, image: &str, size: &str) {
        let key = format!("{}@{}", image, size);
        *self.counts.entry(key).or_default() += 1;
    }

    pub fn top(&self, n: usize) -> Vec<(String, u64)> {
        let mut v: Vec<_> = self.counts.iter().map(|(k, c)| (k.clone(), *c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.truncate(n);
        v
    }
}
