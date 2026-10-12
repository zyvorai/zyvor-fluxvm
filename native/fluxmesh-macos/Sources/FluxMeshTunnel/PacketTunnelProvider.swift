import Foundation
import NetworkExtension

final class PacketTunnelProvider: NEPacketTunnelProvider {
    private let engine = EngineBridge()

    override func startTunnel(options: [String : NSObject]?, completionHandler: @escaping (Error?) -> Void) {
        do {
            guard let proto = protocolConfiguration as? NETunnelProviderProtocol,
                  let config = proto.providerConfiguration,
                  let planPath = config["planPath"] as? String,
                  let engineSocket = config["engineSocket"] as? String else {
                throw NSError(domain: "FluxMesh", code: 1, userInfo: [NSLocalizedDescriptionKey: "missing planPath/engineSocket"])
            }
            let plan = try FluxMeshPlan.load(path: planPath)
            let settings = NEPacketTunnelNetworkSettings(tunnelRemoteAddress: "127.0.0.1")
            settings.mtu = NSNumber(value: plan.mtu)

            let local4 = plan.local_address.split(separator: "/", maxSplits: 1).first.map(String.init) ?? plan.local_address
            let ipv4 = NEIPv4Settings(addresses: [local4], subnetMasks: ["255.255.255.255"])
            ipv4.includedRoutes = plan.peers.flatMap { peer in
                peer.allowed_ips.compactMap { cidr in
                    guard !cidr.contains(":"), let (address, prefix) = splitCIDR(cidr), let bits = Int(prefix) else { return nil }
                    return NEIPv4Route(destinationAddress: address, subnetMask: ipv4Mask(bits: bits))
                }
            }
            settings.ipv4Settings = ipv4

            if let local6 = plan.local_ipv6 {
                let address6 = local6.split(separator: "/", maxSplits: 1).first.map(String.init) ?? local6
                let ipv6 = NEIPv6Settings(addresses: [address6], networkPrefixLengths: [128])
                ipv6.includedRoutes = plan.peers.flatMap { peer in
                    peer.allowed_ips.compactMap { cidr in
                        guard cidr.contains(":"), let (address, prefix) = splitCIDR(cidr), let bits = Int(prefix) else { return nil }
                        return NEIPv6Route(destinationAddress: address, networkPrefixLength: NSNumber(value: bits))
                    }
                }
                settings.ipv6Settings = ipv6
            }

            let dns = NEDNSSettings(servers: [local4])
            dns.matchDomains = [plan.dns_suffix]
            dns.matchDomainsNoSearch = false
            settings.dnsSettings = dns

            setTunnelNetworkSettings(settings) { [weak self] error in
                if let error { completionHandler(error); return }
                guard let self else { completionHandler(nil); return }
                self.engine.start(socketPath: engineSocket, flow: self.packetFlow, completion: completionHandler)
            }
        } catch {
            completionHandler(error)
        }
    }

    override func stopTunnel(with reason: NEProviderStopReason, completionHandler: @escaping () -> Void) {
        engine.stop()
        completionHandler()
    }
}

private func splitCIDR(_ cidr: String) -> (String, String)? {
    let parts = cidr.split(separator: "/", maxSplits: 1).map(String.init)
    return parts.count == 2 ? (parts[0], parts[1]) : nil
}

private func ipv4Mask(bits: Int) -> String {
    let bits = max(0, min(32, bits))
    let value: UInt32 = bits == 0 ? 0 : UInt32.max << UInt32(32 - bits)
    return [24, 16, 8, 0].map { String((value >> UInt32($0)) & 0xff) }.joined(separator: ".")
}
