/// The capture screen's frieze: the fate of the most recent frames, oldest
/// first. It is rebuilt from the engine's counters, so it is a display
/// concern only; nothing here reaches the capture.
public struct Frieze: Equatable {
    public enum Tick: Equatable {
        /// Sent, not yet acknowledged.
        case inFlight
        /// Acknowledged by the server, or written to the file offline.
        case confirmed
        /// Worth sending but dropped (flow control, slow network).
        case dropped
    }

    public static let capacity = 64
    public private(set) var ticks: [Tick] = []

    public init() {}

    /// The tick in a slot, left to right; nil while the frieze is not full.
    public func tick(at slot: Int) -> Tick? {
        let offset = Self.capacity - ticks.count
        return slot >= offset && slot < Self.capacity ? ticks[slot - offset] : nil
    }

    public func count(of kind: Tick) -> Int {
        ticks.filter { $0 == kind }.count
    }

    /// Adds what the counters gained since the last call: frames written to a
    /// file, sent, dropped, and acknowledged (in order, oldest first).
    public mutating func advance(written: Int, sent: Int, dropped: Int, acknowledged: Int) {
        ticks += Array(repeating: .confirmed, count: max(0, written))
        ticks += Array(repeating: .inFlight, count: max(0, sent))
        ticks += Array(repeating: .dropped, count: max(0, dropped))
        var remaining = acknowledged
        var index = 0
        while remaining > 0, index < ticks.count {
            if ticks[index] == .inFlight {
                ticks[index] = .confirmed
                remaining -= 1
            }
            index += 1
        }
        if ticks.count > Self.capacity {
            ticks.removeFirst(ticks.count - Self.capacity)
        }
    }
}
