import XCTest

@testable import SplatCaptureKit

final class FriezeTests: XCTestCase {
    func testAcknowledgementsConfirmTheOldestFramesInFlight() {
        var frieze = Frieze()
        frieze.advance(written: 0, sent: 3, dropped: 0, acknowledged: 0)
        frieze.advance(written: 0, sent: 0, dropped: 1, acknowledged: 0)
        frieze.advance(written: 0, sent: 1, dropped: 0, acknowledged: 2)
        XCTAssertEqual(frieze.ticks, [.confirmed, .confirmed, .inFlight, .dropped, .inFlight])
    }

    func testOfflineFramesAreConfirmedAtOnce() {
        var frieze = Frieze()
        frieze.advance(written: 4, sent: 0, dropped: 0, acknowledged: 0)
        XCTAssertEqual(frieze.count(of: .confirmed), 4)
    }

    func testOnlyTheMostRecentFramesAreKept() {
        var frieze = Frieze()
        frieze.advance(written: 0, sent: 70, dropped: 0, acknowledged: 70)
        frieze.advance(written: 0, sent: 0, dropped: 1, acknowledged: 0)
        XCTAssertEqual(frieze.ticks.count, Frieze.capacity)
        XCTAssertEqual(frieze.ticks.last, .dropped)
        XCTAssertEqual(frieze.tick(at: Frieze.capacity - 1), .dropped)
    }

    func testAPartlyFilledFriezeFillsFromTheRight() {
        var frieze = Frieze()
        frieze.advance(written: 0, sent: 2, dropped: 0, acknowledged: 1)
        XCTAssertNil(frieze.tick(at: 0))
        XCTAssertEqual(frieze.tick(at: Frieze.capacity - 2), .confirmed)
        XCTAssertEqual(frieze.tick(at: Frieze.capacity - 1), .inFlight)
        XCTAssertNil(frieze.tick(at: Frieze.capacity))
    }
}
