import XCTest
@testable import ClawChatMac

final class AppIconTests: XCTestCase {
    func testApplicationIconIsBundledAtProductionResolution() throws {
        let icon = try XCTUnwrap(ClawChatAppDelegate.applicationIcon())
        let representation = try XCTUnwrap(icon.representations.first)

        XCTAssertGreaterThanOrEqual(representation.pixelsWide, 1024)
        XCTAssertGreaterThanOrEqual(representation.pixelsHigh, 1024)
    }
}
