import Foundation
import Network
import NetworkExtension

/// Packet engine bridge framing:
///   1 byte IP family (4 or 6), then the raw IP packet.
/// The helper is expected to return packets with the same framing.
final class EngineBridge {
    private let queue = DispatchQueue(label: "dev.zyvor.fluxmesh.engine")
    private var connection: NWConnection?
    private weak var flow: NEPacketTunnelFlow?

    func start(socketPath: String, flow: NEPacketTunnelFlow, completion: @escaping (Error?) -> Void) {
        self.flow = flow
        let connection = NWConnection(to: .unix(path: socketPath), using: .udp)
        self.connection = connection
        connection.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                self?.receiveLoop()
                self?.readFromTunnel()
                completion(nil)
            case .failed(let error):
                completion(error)
            default:
                break
            }
        }
        connection.start(queue: queue)
    }

    func stop() {
        connection?.cancel()
        connection = nil
    }

    private func readFromTunnel() {
        guard let flow else { return }
        flow.readPackets { [weak self] packets, protocols in
            guard let self, let connection = self.connection else { return }
            for (packet, proto) in zip(packets, protocols) {
                let family: UInt8 = proto.int32Value == AF_INET6 ? 6 : 4
                var frame = Data([family])
                frame.append(packet)
                connection.send(content: frame, completion: .contentProcessed { _ in })
            }
            self.readFromTunnel()
        }
    }

    private func receiveLoop() {
        connection?.receiveMessage { [weak self] data, _, _, error in
            guard let self else { return }
            if let data, data.count > 1, let flow = self.flow {
                let family = data[data.startIndex]
                let packet = data.dropFirst()
                let proto = NSNumber(value: family == 6 ? AF_INET6 : AF_INET)
                flow.writePackets([Data(packet)], withProtocols: [proto])
            }
            if error == nil {
                self.receiveLoop()
            }
        }
    }
}
