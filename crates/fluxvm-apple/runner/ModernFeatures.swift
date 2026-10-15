// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0
import Foundation
import Virtualization
#if canImport(DiskImageKit)
import DiskImageKit
#endif

extension Runner {
    func modernError(_ s: String) -> NSError {
        NSError(domain: "fluxvm-vz", code: 2, userInfo: [NSLocalizedDescriptionKey: s])
    }

    /// Why `action` cannot run now, from VZ's own `canStart/canPause/canResume/canRequestStop/canStop`, or nil when it can.
    /// Gives the control socket a clear string instead of a VZ failure raised mid-transition.
    func controlPrecondition(_ action: String) -> String? {
        guard let machine = vm else { return "cannot \(action): the VM has not been created yet (state: \(state))" }
        let ok: Bool
        let needs: String
        switch action {
        case "start": ok = machine.canStart; needs = "a stopped VM"
        case "pause": ok = machine.canPause; needs = "a running VM"
        case "resume": ok = machine.canResume; needs = "a paused VM"
        case "shutdown": ok = machine.canRequestStop; needs = "a running VM"
        case "stop": ok = machine.canStop; needs = "a VM that has not already stopped"
        default: return nil
        }
        if ok { return nil }
        return "cannot \(action): the VM is \(state); \(action) needs \(needs)"
    }

    /// `display-resize`: `VZGraphicsDisplay.reconfigure(configuration:)` (macOS 14+) for display `display` (default 0).
    /// UNVERIFIED on hardware: whether the guest honours the new size depends on its driver.
    func displayResize(_ o: [String: Any]) -> [String: Any] {
        guard let w = (o["width"] as? NSNumber)?.intValue, let h = (o["height"] as? NSNumber)?.intValue,
              (800...5120).contains(w), (600...2880).contains(h) else {
            return ["ok": false, "error": "display-resize needs width 800..=5120 and height 600..=2880"]
        }
        let index = (o["display"] as? NSNumber)?.intValue ?? 0
        guard #available(macOS 14.0, *) else { return ["ok": false, "error": "display-resize needs macOS 14 or later"] }
        guard let machine = vm else { return ["ok": false, "error": "display-resize needs a created VM (state: \(state))"] }
        guard state == "running" || state == "paused" else {
            return ["ok": false, "error": "display-resize needs a started VM, but it is \(state)"]
        }
        let displays = machine.graphicsDevices.first?.displays ?? []
        guard index >= 0, index < displays.count else {
            return ["ok": false, "error": "display \(index) does not exist (the VM has \(displays.count))"]
        }
        let configuration: VZGraphicsDisplayConfiguration
        if cfg.guest_os == "macos" {
            let ppi = (o["ppi"] as? NSNumber)?.intValue ?? cfg.display_ppi ?? 220
            configuration = VZMacGraphicsDisplayConfiguration(widthInPixels: w, heightInPixels: h, pixelsPerInch: ppi)
        } else {
            configuration = VZVirtioGraphicsScanoutConfiguration(widthInPixels: w, heightInPixels: h)
        }
        do {
            try displays[index].reconfigure(configuration: configuration)
            return ["ok": true, "display": index, "width": w, "height": h]
        } catch {
            return ["ok": false, "error": "display-resize failed: \(fluxVZErrorDescription(error))"]
        }
    }

    func macDisplays() -> [VZMacGraphicsDisplayConfiguration] {
        let count = max(1, min(cfg.display_count ?? 1, 8))
        return (0..<count).map { _ in
            VZMacGraphicsDisplayConfiguration(widthInPixels: cfg.display_width ?? 2560,
                heightInPixels: cfg.display_height ?? 1600, pixelsPerInch: cfg.display_ppi ?? 220)
        }
    }

    func networkAttachment() throws -> VZNetworkDeviceAttachment {
        if let fd = cfg.usernet_fd {
            // apple.usernet: frames go over the inherited datagram socket to the supervised fluxvm-usernet gateway.
            // UNVERIFIED on hardware. The descriptor stays open for the life of the runner (the VM may be rebuilt).
            var snd: Int32 = 1 << 20, rcv: Int32 = 4 << 20
            setsockopt(fd, SOL_SOCKET, SO_SNDBUF, &snd, socklen_t(MemoryLayout<Int32>.size))
            setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &rcv, socklen_t(MemoryLayout<Int32>.size))
            let attachment = VZFileHandleNetworkDeviceAttachment(fileHandle: FileHandle(fileDescriptor: fd, closeOnDealloc: false))
            // Jumbo frames: the gateway advertises `mtu` to the guest (DHCP option 26); Virtualization drops anything larger than
            // the attachment's own MTU (default 1500), so the two must agree.
            if let mtu = cfg.usernet_mtu, mtu > 1500 { attachment.maximumTransmissionUnit = Int(mtu) }
            return attachment
        }
        if let spec = cfg.vmnet {
            #if canImport(vmnet)
            if #available(macOS 26.0, *) {
                return try FluxVMNetNetwork(spec: spec, macAddress: cfg.mac).attachment()
            }
            #endif
            throw modernError("apple.vmnet needs a macOS 26+ host")
        }
        guard let name = cfg.bridge_interface, !name.isEmpty else { return VZNATNetworkDeviceAttachment() }
        guard let iface = VZBridgedNetworkInterface.networkInterfaces.first(where: { $0.identifier == name }) else {
            throw modernError("bridge interface \(name) not found; available: " +
                VZBridgedNetworkInterface.networkInterfaces.map { $0.identifier }.joined(separator: ", "))
        }
        return VZBridgedNetworkDeviceAttachment(interface: iface)
    }

    func configureCustomVirtio(_ c: VZVirtualMachineConfiguration) throws {
        guard cfg.custom_virtio == true else { return }
        guard cfg.guest_os == "linux" else { throw modernError("apple.custom_virtio is Linux-only") }
        #if compiler(>=6.4)
        guard #available(macOS 27.0, *) else { throw modernError("apple.custom_virtio needs a macOS 27+ host") }
        c.customVirtioDevices = [fluxVMCustomVirtioConfiguration(vmID: cfg.id)]
        #else
        throw modernError("apple.custom_virtio needs a runner built with the macOS 27 SDK")
        #endif
    }

    func configureClipboard(_ c: VZVirtualMachineConfiguration) throws {
        guard cfg.clipboard == true else { return }
        guard cfg.guest_os == "linux" else { throw modernError("SPICE clipboard is Linux-only") }
        let a = VZSpiceAgentPortAttachment(); a.sharesClipboard = true
        let p = VZVirtioConsolePortConfiguration(); p.name = VZSpiceAgentPortAttachment.spiceAgentPortName
        p.attachment = a; p.isConsole = false
        let d = VZVirtioConsoleDeviceConfiguration(); d.ports[0] = p
        c.consoleDevices.append(d)
    }

    /// A disk image attachment honouring `root_caching` / `root_sync` (VZ defaults when unset).
    func imageAttachment(_ url: URL, readOnly: Bool) throws -> VZDiskImageStorageDeviceAttachment {
        let caching: VZDiskImageCachingMode = cfg.root_caching == "cached" ? .cached : cfg.root_caching == "uncached" ? .uncached : .automatic
        let sync: VZDiskImageSynchronizationMode = cfg.root_sync == "none" ? .none : cfg.root_sync == "fsync" ? .fsync : .full
        return try VZDiskImageStorageDeviceAttachment(url: url, readOnly: readOnly, cachingMode: caching, synchronizationMode: sync)
    }

    func rootStorageAttachment() throws -> VZStorageDeviceAttachment {
        let u = URL(fileURLWithPath: cfg.disk)
        guard cfg.asif_overlay == true else { return try imageAttachment(u, readOnly: cfg.root_read_only ?? false) }
        #if canImport(DiskImageKit)
        if #available(macOS 27.0, *) {
            let base = try DiskImage(opening: .open(url: u, mode: .readOnly))
            let ov = file("disk-overlay.asif")
            if FileManager.default.fileExists(atPath: ov.path) {
                let overlay = try DiskImage(opening: .open(url: ov))
                return try VZDiskImageStorageDeviceAttachment(diskImage: try base.appending(overlay))
            }
            return try VZDiskImageStorageDeviceAttachment(diskImage: try base.appending(.asifLayer(url: ov, type: .overlay)))
        }
        #endif
        throw modernError("ASIF overlay needs macOS 27+ with DiskImageKit")
    }

    func startMachine(_ machine: VZVirtualMachine) {
        if let why = controlPrecondition("start") { fail(why) }
        // The password file is one-shot: it is deleted once the guest has been provisioned, so a later start of the same
        // VM (stop/start, restart after a crash) must not try to provision again.
        let provision = cfg.provision_username != nil && cfg.provision_full_name != nil
            && (cfg.provision_password_file.map { FileManager.default.fileExists(atPath: $0) } ?? false)
        guard cfg.guest_os == "macos", provision || cfg.recovery == true else {
            machine.start { r in
                switch r { case .success: self.state = "running"; emit(["event":"running"])
                case .failure(let e): fail("start failed: \(fluxVZErrorDescription(e))") }
            }
            return
        }
        let o = VZMacOSVirtualMachineStartOptions()
        o.startUpFromMacOSRecovery = cfg.recovery == true
        if provision { applyProvisioning(o) }
        machine.start(options: o) { e in
            if let e { fail("macOS start failed: \(fluxVZErrorDescription(e))") }
            self.state = "running"
            emit(["event":"running","provisioned":provision,"recovery":o.startUpFromMacOSRecovery])
        }
    }

    private func applyProvisioning(_ o: VZMacOSVirtualMachineStartOptions) {
        guard let user = cfg.provision_username, let full = cfg.provision_full_name,
              let pwFile = cfg.provision_password_file else { return }
        #if compiler(>=6.4)
        if #available(macOS 27.0, *) {
            do {
                let pw = try String(contentsOfFile: pwFile, encoding: .utf8).trimmingCharacters(in: .newlines)
                guard !pw.isEmpty else { fail("provision password file is empty") }
                let p = VZMacGuestProvisioningOptions(); p.fullName = full; p.username = user; p.password = pw
                p.logsInAutomatically = cfg.provision_auto_login == true
                p.enablesRemoteLogin = cfg.provision_remote_login == true
                try o.setGuestProvisioning(p)
                try? FileManager.default.removeItem(atPath: pwFile)
                return
            } catch { fail("macOS provisioning: \(fluxVZErrorDescription(error))") }
        }
        #endif
        fail("automated macOS provisioning needs macOS 27/current SDK")
    }

    /// macOS 27 cannot save a VM with passed-through USB devices (174267926), so they go back to the host first.
    /// A hot-plugged USB disk is refused instead: restoring without it can crash the VM (177528319), and a restore
    /// cannot re-attach it before the guest resumes.
    func prepareUSBForSave(_ done: @escaping (_ detached: [String], _ error: String?) -> Void) {
        guard #available(macOS 15.0, *), let vm else { done([], nil); return }
        let attached = Set(vm.usbControllers.flatMap { $0.usbDevices.map(\.uuid) })
        if !hotplugUSB.intersection(attached).isEmpty {
            done([], "detach the hot-plugged USB disk(s) first (usb-detach): restoring a state saved with them crashes on macOS 27")
            return
        }
        #if compiler(>=6.4)
        guard #available(macOS 27.0, *) else { done([], nil); return }
        var pending = vm.usbControllers.flatMap { c in
            c.usbDevices.compactMap { $0 as? VZUSBPassthroughDevice }.map { (c, $0) }
        }
        var detached: [String] = []
        func next() {
            guard let (c, d) = pending.popLast() else { done(detached, nil); return }
            c.detach(device: d) { e in
                if let e { done(detached, "detaching USB passthrough device \(d.uuid): \(e.localizedDescription)"); return }
                detached.append(d.uuid.uuidString)
                emit(["event": "usb-passthrough-detached-for-save", "uuid": d.uuid.uuidString])
                next()
            }
        }
        next()
        #else
        done([], nil)
        #endif
    }

    func balloonControl(reclaimMiB: UInt64?) -> [String: Any] {
        guard let d = vm?.memoryBalloonDevices.first as? VZVirtioTraditionalMemoryBalloonDevice else {
            return ["ok":false,"error":"no VZ memory balloon device"]
        }
        let configured = cfg.memory_mib
        if let r = reclaimMiB {
            let floor = VZVirtualMachineConfiguration.minimumAllowedMemorySize / 1_048_576
            d.targetVirtualMachineMemorySize = max(floor, configured - min(r, configured)) * 1_048_576
        }
        let available = d.targetVirtualMachineMemorySize / 1_048_576
        let reclaimed = configured > available ? configured - available : 0
        return ["ok":true,"memory_mib":configured,"target_mib":reclaimed,"actual_mib":reclaimed]
    }

    func attachUSBMassStorage(path: String, readOnly: Bool, bus: Int = 0, completion: @escaping ([String: Any])->Void) {
        guard #available(macOS 15.0, *) else { completion(["ok":false,"error":"USB hotplug needs macOS 15+"]); return }
        guard let buses = vm?.usbControllers, !buses.isEmpty else { completion(["ok":false,"error":"no XHCI controller; set apple.usb_controller=true"]); return }
        guard bus >= 0, bus < buses.count else { completion(["ok":false,"error":"usb bus \(bus) does not exist (\(buses.count) controller(s))"]); return }
        let c = buses[bus]
        do {
            let a = try VZDiskImageStorageDeviceAttachment(url: URL(fileURLWithPath: path), readOnly: readOnly)
            let d = VZUSBMassStorageDevice(configuration: VZUSBMassStorageDeviceConfiguration(attachment: a))
            c.attach(device: d) { e in
                guard e == nil else { completion(["ok":false,"error":e!.localizedDescription]); return }
                self.hotplugUSB.insert(d.uuid)
                completion(["ok":true,"uuid":d.uuid.uuidString])
            }
        } catch { completion(["ok":false,"error":error.localizedDescription]) }
    }

    func detachUSB(uuid: String, completion: @escaping ([String: Any])->Void) {
        guard #available(macOS 15.0, *) else { completion(["ok":false,"error":"USB hotplug needs macOS 15+"]); return }
        guard let u = UUID(uuidString: uuid),
              let c = vm?.usbControllers.first(where: { $0.usbDevices.contains { $0.uuid == u } }),
              let d = c.usbDevices.first(where: { $0.uuid == u }) else { completion(["ok":false,"error":"USB device not attached"]); return }
        c.detach(device: d) { e in
            guard e == nil else { completion(["ok":false,"error":e!.localizedDescription]); return }
            self.hotplugUSB.remove(u)
            completion(["ok":true,"uuid":uuid])
        }
    }
}
