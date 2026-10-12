import Foundation

struct FluxMeshPeer: Decodable {
    let node_id: String
    let public_key: String
    let endpoint: String?
    let allowed_ips: [String]
    let persistent_keepalive: UInt16
}

struct FluxMeshPlan: Decodable {
    let version: UInt32
    let local_node: String
    let local_address: String
    let local_ipv6: String?
    let listen_port: UInt16
    let mtu: UInt16
    let dns_suffix: String
    let peers: [FluxMeshPeer]

    static func load(path: String) throws -> FluxMeshPlan {
        let data = try Data(contentsOf: URL(fileURLWithPath: path))
        return try JSONDecoder().decode(FluxMeshPlan.self, from: data)
    }
}
