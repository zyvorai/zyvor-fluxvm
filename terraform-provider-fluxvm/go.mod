module github.com/zyvorai/fluxvm/terraform-provider-fluxvm

go 1.23

require github.com/zyvorai/fluxvm/go v0.0.0

require (
	github.com/fatih/color v1.13.0 // indirect
	github.com/golang/protobuf v1.5.4 // indirect
	github.com/hashicorp/go-hclog v1.5.0 // indirect
	github.com/hashicorp/go-plugin v1.6.2 // indirect
	github.com/hashicorp/go-uuid v1.0.3 // indirect
	github.com/hashicorp/terraform-plugin-framework v1.13.0
	github.com/hashicorp/terraform-plugin-go v0.25.0
	github.com/hashicorp/terraform-plugin-log v0.9.0 // indirect
	github.com/hashicorp/terraform-registry-address v0.2.3 // indirect
	github.com/hashicorp/terraform-svchost v0.1.1 // indirect
	github.com/hashicorp/yamux v0.1.1 // indirect
	github.com/mattn/go-colorable v0.1.12 // indirect
	github.com/mattn/go-isatty v0.0.17 // indirect
	github.com/mitchellh/go-testing-interface v1.14.1 // indirect
	github.com/oklog/run v1.0.0 // indirect
	github.com/vmihailenco/msgpack/v5 v5.4.1 // indirect
	github.com/vmihailenco/tagparser/v2 v2.0.0 // indirect
	golang.org/x/net v0.28.0 // indirect
	golang.org/x/sys v0.24.0 // indirect
	golang.org/x/text v0.17.0 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20240814211410-ddb44dafa142 // indirect
	google.golang.org/grpc v1.67.1 // indirect
	google.golang.org/protobuf v1.35.1 // indirect
)

replace github.com/zyvorai/fluxvm/go => ../go

// golang.org and google.golang.org are not reachable from every build host; their GitHub mirrors are the same code.
replace (
	golang.org/x/net => github.com/golang/net v0.28.0
	golang.org/x/sys => github.com/golang/sys v0.24.0
	golang.org/x/text => github.com/golang/text v0.17.0
	google.golang.org/genproto/googleapis/rpc => github.com/googleapis/go-genproto/googleapis/rpc v0.0.0-20240814211410-ddb44dafa142
	google.golang.org/grpc => github.com/grpc/grpc-go v1.67.1
	google.golang.org/protobuf => github.com/protocolbuffers/protobuf-go v1.35.1
	gopkg.in/check.v1 => github.com/go-check/check v0.0.0-20161208181325-20d25e280405
	gopkg.in/yaml.v3 => github.com/go-yaml/yaml/v3 v3.0.1
)
