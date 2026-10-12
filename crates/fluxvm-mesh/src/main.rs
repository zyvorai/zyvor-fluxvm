// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

use fluxvm_mesh::acl::{
    AclAction, AclContext, AclProtocol, AclRule, DestinationSelector, SourceSelector, evaluate,
};
use fluxvm_mesh::catalog::{CatalogStore, CloudCatalog, default_catalog_path};
use fluxvm_mesh::cloud::{
    CloudPaths, accept_join_token, issue_join_token, issue_open_join_token, status as cloud_status,
    up as cloud_up,
};
use fluxvm_mesh::dns::PrivateDns;
use fluxvm_mesh::ipv6::Ipv6Overlay;
use fluxvm_mesh::keydist::{KeyDistributionStore, default_keydist_path};
use fluxvm_mesh::relay::{RelayRecord, RelayTransport};
use fluxvm_mesh::service::{EndpointHealth, ServiceEndpoint, ServiceProtocol};
use fluxvm_mesh::{
    DEFAULT_POOL, Endpoint, EndpointKind, MeshError, MeshState, NatObservation, PathSignals,
    RouteAdvertisement, RouteKind, StateStore, classify_nat, default_state_path,
};
use std::env;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

fn usage() -> ! {
    eprintln!(
        "fluxmesh - FluxVM personal-cloud overlay control plane\n\n\
USAGE:\n\
  # Phase 1: connectivity\n\
  fluxmesh init [--state PATH] NODE_ID HOSTNAME PUBLIC_KEY [POOL]\n\
  fluxmesh status [--state PATH]\n\
  fluxmesh peer-add [--state PATH] NODE_ID HOSTNAME PUBLIC_KEY [--relay]\n\
  fluxmesh endpoint-add [--state PATH] NODE_ID KIND IP:PORT [PRIORITY]\n\
  fluxmesh route-add [--state PATH] CIDR VIA_NODE KIND [METRIC]\n\
  fluxmesh route-del [--state PATH] CIDR VIA_NODE\n\
  fluxmesh path [--state PATH] NODE_ID [--lan] [--ipv6] [--udp] [--peer-relay] [--relay443]\n\
  fluxmesh nat-classify MAPPED_ADDR...\n\
  fluxmesh wg-plan [--state PATH] INTERFACE LISTEN_PORT PRIVATE_KEY_FILE\n\n\
  # Phase 2: personal-cloud services/policy\n\
  fluxmesh catalog-status [--state PATH] [--catalog PATH]\n\
  fluxmesh service-add [--state PATH] [--catalog PATH] NAME PORT tcp|udp\n\
  fluxmesh service-endpoint-add [--state PATH] [--catalog PATH] SERVICE NODE IP PORT [WEIGHT] [HEALTH]\n\
  fluxmesh service-endpoint-health [--state PATH] [--catalog PATH] SERVICE NODE IP PORT HEALTH\n\
  fluxmesh service-list [--state PATH] [--catalog PATH]\n\
  fluxmesh service-fabric-plan [--state PATH] [--catalog PATH] SERVICE\n\
  fluxmesh dns [--state PATH] [--catalog PATH] NAME\n\
  fluxmesh dns-serve [--state PATH] [--catalog PATH] BIND_ADDR\n\
  fluxmesh acl-add [--state PATH] [--catalog PATH] ID PRIORITY allow|deny SOURCE DEST PROTO [PORT]\n\
  fluxmesh acl-del [--state PATH] [--catalog PATH] ID\n\
  fluxmesh acl-list [--state PATH] [--catalog PATH]\n\
  fluxmesh acl-check [--state PATH] [--catalog PATH] SOURCE_NODE DEST PROTO [PORT]\n\
  fluxmesh exit-enable [--state PATH] [--catalog PATH] EXIT_NODE [ALLOWED_NODE...]\n\
  fluxmesh exit-disable [--state PATH] [--catalog PATH] EXIT_NODE\n\
  fluxmesh subnet-policy [--state PATH] [--catalog PATH] CIDR VIA_NODE [ALLOWED_NODE...]\n\
  fluxmesh gateway-plan [--state PATH] [--catalog PATH] NODE MESH_IF UPLINK_IF\n\
  fluxmesh relay-add [--state PATH] [--catalog PATH] ID udp|tcp443 IP:PORT [PRIORITY]\n\
  fluxmesh relay-health [--state PATH] [--catalog PATH] ID up|down [LOAD_PERCENT]\n\
  fluxmesh relay-select [--state PATH] [--catalog PATH] udp|tcp443\n\n\
  # Phase 4: zero-touch personal cloud\n\
  fluxmesh cloud-up [--state PATH] [--catalog PATH] [--key PATH] NODE HOSTNAME [POOL]\n\
  fluxmesh cloud-status [--state PATH] [--catalog PATH]\n\
  fluxmesh enroll-token [--state PATH] NODE HOSTNAME PUBLIC_KEY [TTL_SECONDS]\n\
  fluxmesh enroll-accept [--state PATH] TOKEN\n\
  fluxmesh key-propose [--state PATH] NODE PUBLIC_KEY\n\
  fluxmesh key-ack [--state PATH] OWNER GENERATION PEER\n\
  fluxmesh key-activate [--state PATH] OWNER\n\
  fluxmesh ipv6-status [--state PATH]\n\n\
KIND:\n\
  endpoint: lan | public_ipv6 | public_udp | peer_relay | relay443\n\
  route:    host | vm_subnet | lan_subnet | exit\n\n\
ACL selectors:\n\
  SOURCE: any | node:<id>\n\
  DEST:   any | node:<id> | service:<name> | cidr:<prefix>\n\
  acl-check DEST additionally accepts ip:<address>\n\n\
STATE:\n\
  $FLUXMESH_STATE   or <mesh dir>/state.v1\n\
  $FLUXMESH_CATALOG or <mesh dir>/catalog.v1\n\
  <mesh dir> is $FLUXMESH_DIR, else /var/lib/fluxvm/mesh (Linux) or\n\
  ~/Library/Application Support/FluxVM/mesh (macOS)"
    );
    std::process::exit(2);
}

fn take_path(args: &mut Vec<String>, flag: &str, default: impl FnOnce() -> PathBuf) -> PathBuf {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            if i + 1 >= args.len() {
                usage();
            }
            let path = PathBuf::from(args.remove(i + 1));
            args.remove(i);
            return path;
        }
        i += 1;
    }
    default()
}

fn load(path: &PathBuf) -> fluxvm_mesh::Result<MeshState> {
    StateStore::new(path).load()
}

fn save(path: &PathBuf, state: &MeshState) -> fluxvm_mesh::Result<()> {
    StateStore::new(path).save(state)
}

fn load_catalog(path: &PathBuf, mesh: &MeshState) -> fluxvm_mesh::Result<CloudCatalog> {
    CatalogStore::new(path).load(mesh)
}

fn save_catalog(path: &PathBuf, catalog: &CloudCatalog) -> fluxvm_mesh::Result<()> {
    CatalogStore::new(path).save(catalog)
}

fn print_status(state: &MeshState) {
    println!("FluxMesh state v{}", state.version);
    println!("local: {}", state.local_node);
    println!("pool: {}", state.ipam.pool);
    println!("nodes: {}", state.nodes.len());
    println!("routes: {}", state.routes.len());
    println!();
    println!("NODE\tOVERLAY\tRELAY\tENDPOINTS\tHOSTNAME");
    for node in state.nodes.values() {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            node.id,
            node.overlay_ip,
            if node.relay_capable { "yes" } else { "no" },
            node.endpoints.len(),
            node.hostname
        );
    }
    if !state.routes.is_empty() {
        println!();
        println!("PREFIX\tVIA\tKIND\tMETRIC");
        for route in &state.routes {
            println!(
                "{}\t{}\t{:?}\t{}",
                route.prefix, route.via_node, route.kind, route.metric
            );
        }
    }
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("fluxmesh: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> fluxvm_mesh::Result<()> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    let command = args.remove(0);
    if command == "nat-classify" {
        return cmd_nat_classify(args);
    }
    let state_path = take_path(&mut args, "--state", default_state_path);
    let catalog_path = take_path(&mut args, "--catalog", default_catalog_path);
    let key_path = take_path(&mut args, "--key", || {
        fluxvm_mesh::mesh_dir().join("wireguard.key")
    });
    let mut cloud_paths = CloudPaths::default();
    cloud_paths.state = state_path.clone();
    cloud_paths.catalog = catalog_path.clone();
    cloud_paths.key = key_path;

    match command.as_str() {
        "init" => {
            if !(3..=4).contains(&args.len()) {
                usage();
            }
            let pool = args
                .get(3)
                .map(String::as_str)
                .unwrap_or(DEFAULT_POOL)
                .parse()?;
            let state = MeshState::new(pool, &args[0], &args[1], &args[2])?;
            save(&state_path, &state)?;
            let local = state.node(&state.local_node)?;
            println!("initialized {} at {}", local.id, local.overlay_ip);
            println!("state: {}", state_path.display());
        }
        "status" => {
            if !args.is_empty() {
                usage();
            }
            print_status(&load(&state_path)?);
        }
        "peer-add" => {
            let relay = if let Some(pos) = args.iter().position(|a| a == "--relay") {
                args.remove(pos);
                true
            } else {
                false
            };
            if args.len() != 3 {
                usage();
            }
            let mut state = load(&state_path)?;
            let ip = state.add_node(&args[0], &args[1], &args[2], relay)?;
            save(&state_path, &state)?;
            println!("peer {} -> {}", args[0], ip);
        }
        "endpoint-add" => {
            if !(3..=4).contains(&args.len()) {
                usage();
            }
            let kind = EndpointKind::from_str(&args[1])?;
            let addr: SocketAddr = parse(&args[2], "endpoint address")?;
            let priority = args
                .get(3)
                .map(|v| parse(v, "priority"))
                .transpose()?
                .unwrap_or(100);
            let mut state = load(&state_path)?;
            state.add_endpoint(
                &args[0],
                Endpoint {
                    kind,
                    addr,
                    priority,
                },
            )?;
            save(&state_path, &state)?;
            println!("endpoint added to {}", args[0]);
        }
        "route-add" => {
            if !(3..=4).contains(&args.len()) {
                usage();
            }
            let metric = args
                .get(3)
                .map(|v| parse(v, "route metric"))
                .transpose()?
                .unwrap_or(100);
            let route = RouteAdvertisement {
                prefix: args[0].parse()?,
                via_node: args[1].clone(),
                kind: RouteKind::from_str(&args[2])?,
                metric,
            };
            let mut state = load(&state_path)?;
            state.advertise_route(route)?;
            save(&state_path, &state)?;
            println!("route added");
        }
        "route-del" => {
            if args.len() != 2 {
                usage();
            }
            let prefix = args[0].parse()?;
            let mut state = load(&state_path)?;
            if state.withdraw_route(prefix, &args[1]) {
                save(&state_path, &state)?;
                println!("route removed");
            } else {
                println!("route not present");
            }
        }
        "path" => cmd_path(args, &state_path)?,
        "wg-plan" => {
            if args.len() != 3 {
                usage();
            }
            let listen_port = parse::<u16>(&args[1], "listen port")?;
            let state = load(&state_path)?;
            let mut plan = state.wireguard_plan(&args[0], listen_port, &args[2])?;
            fluxvm_mesh::ipv6::augment_wireguard_plan(&state, &mut plan)?;
            for argv in plan.linux_argv() {
                println!("{}", shell_preview(&argv));
            }
        }
        "cloud-up" => {
            if !(2..=3).contains(&args.len()) {
                usage();
            }
            let status = cloud_up(
                &cloud_paths,
                &args[0],
                &args[1],
                args.get(2).map(String::as_str),
            )?;
            println!(
                "cloud={} ipv4={} ipv6={} nodes={} services={} relays={}",
                status.local_node,
                status.ipv4,
                status.ipv6,
                status.nodes,
                status.services,
                status.relays
            );
        }
        "cloud-status" => {
            if !args.is_empty() {
                usage();
            }
            let status = cloud_status(&cloud_paths)?;
            println!(
                "cloud={} ipv4={} ipv6={} nodes={} services={} relays={} pending_keys={}",
                status.local_node,
                status.ipv4,
                status.ipv6,
                status.nodes,
                status.services,
                status.relays,
                status.pending_key_rotations
            );
        }
        "enroll-token" => {
            if !(3..=4).contains(&args.len()) {
                usage();
            }
            let ttl = args
                .get(3)
                .map(|v| parse::<u64>(v, "TTL seconds"))
                .transpose()?
                .unwrap_or(900);
            println!(
                "{}",
                issue_join_token(
                    &cloud_paths,
                    &args[0],
                    &args[1],
                    &args[2],
                    Duration::from_secs(ttl)
                )?
            );
        }
        "enroll-invite" => {
            if !(2..=3).contains(&args.len()) {
                usage();
            }
            let ttl = args
                .get(2)
                .map(|v| parse::<u64>(v, "TTL seconds"))
                .transpose()?
                .unwrap_or(900);
            println!(
                "{}",
                issue_open_join_token(&cloud_paths, &args[0], &args[1], Duration::from_secs(ttl))?
            );
        }
        "enroll-accept" => {
            if args.len() != 1 {
                usage();
            }
            let ip = accept_join_token(&cloud_paths, &args[0])?;
            println!("enrolled ipv4={ip}");
        }
        "key-propose" => {
            if args.len() != 2 {
                usage();
            }
            let mesh = load(&state_path)?;
            let store = KeyDistributionStore::new(default_keydist_path());
            let mut keys = store.load()?;
            let generation = keys.propose(&mesh, &args[0], args[1].clone())?;
            store.save(&keys)?;
            println!("owner={} generation={generation}", args[0]);
        }
        "key-ack" => {
            if args.len() != 3 {
                usage();
            }
            let mesh = load(&state_path)?;
            let store = KeyDistributionStore::new(default_keydist_path());
            let mut keys = store.load()?;
            keys.acknowledge(&mesh, &args[0], parse(&args[1], "generation")?, &args[2])?;
            store.save(&keys)?;
            println!("acknowledged");
        }
        "key-activate" => {
            if args.len() != 1 {
                usage();
            }
            let state_store = StateStore::new(&state_path);
            let mut mesh = state_store.load()?;
            let store = KeyDistributionStore::new(default_keydist_path());
            let mut keys = store.load()?;
            let public_key = keys.activate(&mut mesh, &args[0])?;
            store.save(&keys)?;
            state_store.save(&mesh)?;
            println!("activated owner={} public_key={public_key}", args[0]);
        }
        "ipv6-status" => {
            if !args.is_empty() {
                usage();
            }
            let mesh = load(&state_path)?;
            let overlay = Ipv6Overlay::for_mesh(&mesh);
            println!("prefix={}", overlay.cidr());
            for node in mesh.nodes.keys() {
                println!("{}\t{}", node, overlay.address_for(node)?);
            }
        }
        "catalog-status" => {
            if !args.is_empty() {
                usage();
            }
            let mesh = load(&state_path)?;
            let catalog = load_catalog(&catalog_path, &mesh)?;
            println!("FluxMesh cloud catalog v{}", catalog.version);
            println!("dns suffix: {}", catalog.dns_suffix);
            println!("service pool: {}", catalog.services.ipam.pool);
            println!("services: {}", catalog.services.services.len());
            println!("acl rules: {}", catalog.acls.len());
            println!("exit nodes: {}", catalog.gateways.exits.len());
            println!("subnet policies: {}", catalog.gateways.subnets.len());
            println!("relays: {}", catalog.relays.relays.len());
        }
        "service-add" => {
            if args.len() != 3 {
                usage();
            }
            let mesh = load(&state_path)?;
            let mut catalog = load_catalog(&catalog_path, &mesh)?;
            let vip = catalog.services.add_service(
                &args[0],
                parse(&args[1], "service port")?,
                ServiceProtocol::from_str(&args[2])?,
            )?;
            save_catalog(&catalog_path, &catalog)?;
            println!("{} -> {}", args[0], vip);
        }
        "service-endpoint-add" => {
            if !(4..=6).contains(&args.len()) {
                usage();
            }
            let mesh = load(&state_path)?;
            let mut catalog = load_catalog(&catalog_path, &mesh)?;
            catalog.services.add_endpoint(
                &mesh,
                &args[0],
                ServiceEndpoint {
                    node_id: args[1].clone(),
                    address: parse(&args[2], "service endpoint address")?,
                    port: parse(&args[3], "service endpoint port")?,
                    weight: args
                        .get(4)
                        .map(|v| parse(v, "weight"))
                        .transpose()?
                        .unwrap_or(1),
                    health: args
                        .get(5)
                        .map(|v| EndpointHealth::from_str(v))
                        .transpose()?
                        .unwrap_or(EndpointHealth::Ready),
                },
            )?;
            save_catalog(&catalog_path, &catalog)?;
            println!("service endpoint added");
        }
        "service-endpoint-health" => {
            if args.len() != 5 {
                usage();
            }
            let mesh = load(&state_path)?;
            let mut catalog = load_catalog(&catalog_path, &mesh)?;
            catalog.services.set_endpoint_health(
                &args[0],
                &args[1],
                parse(&args[2], "service endpoint address")?,
                parse(&args[3], "service endpoint port")?,
                EndpointHealth::from_str(&args[4])?,
            )?;
            save_catalog(&catalog_path, &catalog)?;
            println!("service endpoint health updated");
        }
        "service-list" => {
            if !args.is_empty() {
                usage();
            }
            let mesh = load(&state_path)?;
            let catalog = load_catalog(&catalog_path, &mesh)?;
            println!("NAME\tVIP\tPORT\tPROTO\tENDPOINTS\tREADY");
            for svc in catalog.services.services.values() {
                let ready = svc
                    .endpoints
                    .iter()
                    .filter(|e| e.health == EndpointHealth::Ready)
                    .count();
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    svc.name,
                    svc.vip,
                    svc.port,
                    svc.protocol,
                    svc.endpoints.len(),
                    ready
                );
            }
        }
        "service-fabric-plan" => {
            if args.len() != 1 {
                usage();
            }
            let mesh = load(&state_path)?;
            let catalog = load_catalog(&catalog_path, &mesh)?;
            println!("{}", catalog.services.plan(&args[0])?.to_json());
        }
        "dns" => {
            if args.len() != 1 {
                usage();
            }
            let mesh = load(&state_path)?;
            let catalog = load_catalog(&catalog_path, &mesh)?;
            let dns = PrivateDns::build(&mesh, &catalog.services, &catalog.dns_suffix)?;
            match dns.lookup(&args[0]) {
                Some(fluxvm_mesh::dns::DnsLookup::A(ip)) => println!("A\t{}", ip),
                Some(fluxvm_mesh::dns::DnsLookup::Srv {
                    priority,
                    weight,
                    port,
                    target,
                }) => {
                    println!("SRV\t{}\t{}\t{}\t{}", priority, weight, port, target)
                }
                None => println!("NXDOMAIN"),
            }
        }
        "dns-serve" => {
            if args.len() != 1 {
                usage();
            }
            let mesh = load(&state_path)?;
            let catalog = load_catalog(&catalog_path, &mesh)?;
            let dns = PrivateDns::build(&mesh, &catalog.services, &catalog.dns_suffix)?;
            fluxvm_mesh::dns::serve_udp(parse(&args[0], "DNS bind address")?, dns)?;
        }
        "acl-add" => {
            if !(6..=7).contains(&args.len()) {
                usage();
            }
            let mesh = load(&state_path)?;
            let mut catalog = load_catalog(&catalog_path, &mesh)?;
            let rule = AclRule {
                id: args[0].clone(),
                priority: parse(&args[1], "ACL priority")?,
                action: AclAction::from_str(&args[2])?,
                source: SourceSelector::from_str(&args[3])?,
                destination: DestinationSelector::from_str(&args[4])?,
                protocol: AclProtocol::from_str(&args[5])?,
                port: args.get(6).map(|v| parse(v, "ACL port")).transpose()?,
            };
            catalog.upsert_acl(rule)?;
            save_catalog(&catalog_path, &catalog)?;
            println!("ACL updated");
        }
        "acl-del" => {
            if args.len() != 1 {
                usage();
            }
            let mesh = load(&state_path)?;
            let mut catalog = load_catalog(&catalog_path, &mesh)?;
            if catalog.remove_acl(&args[0]) {
                save_catalog(&catalog_path, &catalog)?;
                println!("ACL removed");
            } else {
                println!("ACL not present");
            }
        }
        "acl-list" => {
            if !args.is_empty() {
                usage();
            }
            let mesh = load(&state_path)?;
            let catalog = load_catalog(&catalog_path, &mesh)?;
            println!("PRIORITY\tACTION\tSOURCE\tDEST\tPROTO\tPORT\tID");
            for r in &catalog.acls {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    r.priority,
                    r.action.as_str(),
                    r.source,
                    r.destination,
                    r.protocol.as_str(),
                    r.port.map(|p| p.to_string()).unwrap_or_else(|| "*".into()),
                    r.id
                );
            }
        }
        "acl-check" => cmd_acl_check(args, &state_path, &catalog_path)?,
        "exit-enable" => {
            if args.is_empty() {
                usage();
            }
            let mesh = load(&state_path)?;
            let mut catalog = load_catalog(&catalog_path, &mesh)?;
            catalog
                .gateways
                .enable_exit(&mesh, &args[0], args[1..].iter().map(String::as_str))?;
            save_catalog(&catalog_path, &catalog)?;
            println!("exit node enabled: {}", args[0]);
        }
        "exit-disable" => {
            if args.len() != 1 {
                usage();
            }
            let mesh = load(&state_path)?;
            let mut catalog = load_catalog(&catalog_path, &mesh)?;
            catalog.gateways.disable_exit(&args[0]);
            save_catalog(&catalog_path, &catalog)?;
            println!("exit node disabled: {}", args[0]);
        }
        "subnet-policy" => {
            if args.len() < 2 {
                usage();
            }
            let mesh = load(&state_path)?;
            let mut catalog = load_catalog(&catalog_path, &mesh)?;
            catalog.gateways.set_subnet(
                &mesh,
                args[0].parse()?,
                &args[1],
                args[2..].iter().map(String::as_str),
            )?;
            save_catalog(&catalog_path, &catalog)?;
            println!("subnet policy updated");
        }
        "gateway-plan" => {
            if args.len() != 3 {
                usage();
            }
            let mesh = load(&state_path)?;
            let catalog = load_catalog(&catalog_path, &mesh)?;
            let plan = catalog
                .gateways
                .linux_plan(&mesh, &args[0], &args[1], &args[2])?;
            for argv in plan.sysctl_argv {
                println!("{}", shell_preview(&argv));
            }
            println!("nft -f - <<'FLUXMESH_NFT'\n{}FLUXMESH_NFT", plan.nft_script);
        }
        "relay-add" => {
            if !(3..=4).contains(&args.len()) {
                usage();
            }
            let mesh = load(&state_path)?;
            let mut catalog = load_catalog(&catalog_path, &mesh)?;
            catalog.relays.upsert(RelayRecord {
                id: args[0].clone(),
                transport: RelayTransport::from_str(&args[1])?,
                endpoint: parse(&args[2], "relay endpoint")?,
                priority: args
                    .get(3)
                    .map(|v| parse(v, "relay priority"))
                    .transpose()?
                    .unwrap_or(100),
                load_percent: 0,
                healthy: true,
            })?;
            save_catalog(&catalog_path, &catalog)?;
            println!("relay added: {}", args[0]);
        }
        "relay-health" => {
            if !(2..=3).contains(&args.len()) {
                usage();
            }
            let mesh = load(&state_path)?;
            let mut catalog = load_catalog(&catalog_path, &mesh)?;
            let relay = catalog
                .relays
                .relays
                .get_mut(&args[0])
                .ok_or_else(|| MeshError::InvalidState(format!("unknown relay {}", args[0])))?;
            relay.healthy = match args[1].as_str() {
                "up" | "healthy" => true,
                "down" | "unhealthy" => false,
                _ => usage(),
            };
            if let Some(load) = args.get(2) {
                relay.load_percent = parse(load, "relay load")?;
                if relay.load_percent > 100 {
                    return Err(MeshError::InvalidState("relay load must be 0..100".into()));
                }
            }
            save_catalog(&catalog_path, &catalog)?;
            println!("relay health updated");
        }
        "relay-select" => {
            if args.len() != 1 {
                usage();
            }
            let mesh = load(&state_path)?;
            let catalog = load_catalog(&catalog_path, &mesh)?;
            let preferred = RelayTransport::from_str(&args[0])?;
            if let Some(relay) = catalog.relays.select(preferred) {
                println!(
                    "{}\t{}\t{}\tpriority={}\tload={}%%",
                    relay.id, relay.transport, relay.endpoint, relay.priority, relay.load_percent
                );
            } else {
                println!("no healthy relay");
            }
        }
        _ => usage(),
    }
    Ok(())
}

fn cmd_nat_classify(args: Vec<String>) -> fluxvm_mesh::Result<()> {
    if args.is_empty() {
        usage();
    }
    let mut obs = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        let mapped_addr = parse(arg, "mapped socket address")?;
        obs.push(NatObservation {
            observer: format!("observer-{i}"),
            mapped_addr,
        });
    }
    println!("{}", classify_nat(&obs));
    Ok(())
}

fn cmd_path(args: Vec<String>, state_path: &PathBuf) -> fluxvm_mesh::Result<()> {
    if args.is_empty() {
        usage();
    }
    let mut args = args;
    let node_id = args.remove(0);
    let signals = PathSignals {
        lan_reachable: args.iter().any(|v| v == "--lan"),
        public_ipv6_reachable: args.iter().any(|v| v == "--ipv6"),
        direct_udp_reachable: args.iter().any(|v| v == "--udp"),
        peer_relay_reachable: args.iter().any(|v| v == "--peer-relay"),
        relay443_reachable: args.iter().any(|v| v == "--relay443"),
    };
    if args.iter().any(|v| {
        !matches!(
            v.as_str(),
            "--lan" | "--ipv6" | "--udp" | "--peer-relay" | "--relay443"
        )
    }) {
        usage();
    }
    let state = load(state_path)?;
    let paths = state.best_paths(&node_id, &signals)?;
    if paths.is_empty() {
        println!("no viable path");
    } else {
        for path in paths {
            println!(
                "{}\tscore={}\tendpoint={}\t{}",
                match path.kind {
                    EndpointKind::Lan => "lan",
                    EndpointKind::PublicIpv6 => "public_ipv6",
                    EndpointKind::PublicUdp => "public_udp",
                    EndpointKind::PeerRelay => "peer_relay",
                    EndpointKind::Relay443 => "relay443",
                },
                path.score,
                path.endpoint
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into()),
                path.reason
            );
        }
    }
    Ok(())
}

fn cmd_acl_check(
    args: Vec<String>,
    state_path: &PathBuf,
    catalog_path: &PathBuf,
) -> fluxvm_mesh::Result<()> {
    if !(3..=4).contains(&args.len()) {
        usage();
    }
    let mesh = load(state_path)?;
    mesh.node(&args[0])?;
    let catalog = load_catalog(catalog_path, &mesh)?;
    let protocol = AclProtocol::from_str(&args[2])?;
    let port = args.get(3).map(|v| parse(v, "ACL port")).transpose()?;
    let (destination_node, destination_service, destination_ip) =
        if let Some(v) = args[1].strip_prefix("node:") {
            mesh.node(v)?;
            (Some(v), None, None)
        } else if let Some(v) = args[1].strip_prefix("service:") {
            catalog.services.service(v)?;
            (None, Some(v), None)
        } else if let Some(v) = args[1].strip_prefix("ip:") {
            (
                None,
                None,
                Some(parse::<Ipv4Addr>(v, "ACL destination IP")?),
            )
        } else if args[1] == "any" || args[1] == "*" {
            (None, None, None)
        } else {
            return Err(MeshError::InvalidState(
                "acl-check destination must be node:, service:, ip:, or any".into(),
            ));
        };
    let action = evaluate(
        &catalog.acls,
        &AclContext {
            source_node: &args[0],
            destination_node,
            destination_service,
            destination_ip,
            protocol,
            port,
        },
    );
    println!("{}", action.as_str());
    Ok(())
}

fn parse<T: FromStr>(value: &str, what: &str) -> fluxvm_mesh::Result<T> {
    value
        .parse()
        .map_err(|_| MeshError::InvalidState(format!("invalid {what}: {value}")))
}

fn shell_preview(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if arg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._/:,=[]".contains(&b))
            {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
