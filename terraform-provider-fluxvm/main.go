// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

// Command terraform-provider-fluxvm is the Terraform provider for the FluxVM Mac cloud (fluxvm-agent central).
package main

import (
	"context"
	"flag"
	"log"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"

	"github.com/zyvorai/fluxvm/terraform-provider-fluxvm/internal/provider"
)

// version is set by the release build (-ldflags "-X main.version=...").
var version = "dev"

func main() {
	var debug bool
	flag.BoolVar(&debug, "debug", false, "run the provider with support for debuggers like delve")
	flag.Parse()
	err := providerserver.Serve(context.Background(), provider.New(version), providerserver.ServeOpts{
		Address: "registry.terraform.io/zyvorai/fluxvm",
		Debug:   debug,
	})
	if err != nil {
		log.Fatal(err)
	}
}
