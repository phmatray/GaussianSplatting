import Foundation

/// The phone never has more than `max_inflight` frames unacknowledged; it
/// skips frames instead of queueing them.
public struct FlowControl {
    /// 0 while no `welcome` was received on the current connection.
    public private(set) var maxInflight: Int = 0
    /// Indices of the frames sent (or being encoded) and not yet acknowledged.
    public private(set) var inflight: Set<UInt32> = []

    /// The phone never uses a larger window than this, whatever the server
    /// offers: every frame in flight sits in memory until the network has
    /// taken it, so the window bounds the memory used when Wi-Fi is slow.
    public static let largestWindow = 16

    public init() {}

    /// The server said `welcome` (on a new connection: nothing is in flight).
    public mutating func welcome(maxInflight: Int) {
        self.maxInflight = min(FlowControl.largestWindow, max(1, maxInflight))
        inflight.removeAll()
    }

    /// The connection is gone: frames in flight will never be acknowledged.
    public mutating func disconnect() {
        maxInflight = 0
        inflight.removeAll()
    }

    /// A `welcome` was received and the connection is still there.
    public var isOpen: Bool {
        return maxInflight > 0
    }

    public var canSend: Bool {
        return maxInflight > 0 && inflight.count < maxInflight
    }

    /// Takes a place in the window for frame `index`; false if there is none.
    public mutating func reserve(_ index: UInt32) -> Bool {
        guard canSend else {
            return false
        }
        inflight.insert(index)
        return true
    }

    /// Whether frame `index` still holds a place (it does not after a
    /// reconnection).
    public func holds(_ index: UInt32) -> Bool {
        return inflight.contains(index)
    }

    /// Gives the place back without an ack (the frame was never sent).
    public mutating func cancel(_ index: UInt32) {
        inflight.remove(index)
    }

    /// An `ack` arrived; false if that frame was not in flight.
    public mutating func acknowledge(_ index: UInt32) -> Bool {
        return inflight.remove(index) != nil
    }
}
