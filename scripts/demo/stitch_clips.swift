import Foundation
import AVFoundation
import CoreMedia

guard CommandLine.arguments.count >= 3 else {
    fputs("Usage: stitch_clips <output.mp4> <clip1.mp4> [clip2.mp4 ...]\n", stderr)
    exit(1)
}

let outURL = URL(fileURLWithPath: CommandLine.arguments[1])
let clipPaths = Array(CommandLine.arguments.dropFirst(2))
try? FileManager.default.removeItem(at: outURL)

// Fast path: single clip -> direct file copy
if clipPaths.count == 1 {
    try FileManager.default.copyItem(at: URL(fileURLWithPath: clipPaths[0]), to: outURL)
    print("Copied single clip -> \(outURL.path)")
    exit(0)
}

let sema = DispatchSemaphore(value: 0)
var exitCode: Int32 = 0

Task {
    do {
        let comp = AVMutableComposition()
        guard let compV = comp.addMutableTrack(withMediaType: .video, preferredTrackID: kCMPersistentTrackID_Invalid) else {
            throw NSError(domain: "stitch", code: 1, userInfo: [NSLocalizedDescriptionKey: "Cannot create video track"])
        }
        var compA: AVMutableCompositionTrack? = nil
        var cursor: CMTime = .zero

        for p in clipPaths {
            let asset = AVURLAsset(url: URL(fileURLWithPath: p))
            let vTracks = try await asset.loadTracks(withMediaType: .video)
            guard let vt = vTracks.first else {
                throw NSError(domain: "stitch", code: 2, userInfo: [NSLocalizedDescriptionKey: "No video track in \(p)"])
            }
            let vRange = try await vt.load(.timeRange)
            let clipDur = vRange.duration
            try compV.insertTimeRange(CMTimeRange(start: .zero, duration: clipDur), of: vt, at: cursor)

            let aTracks = try await asset.loadTracks(withMediaType: .audio)
            if let at = aTracks.first {
                if compA == nil {
                    compA = comp.addMutableTrack(withMediaType: .audio, preferredTrackID: kCMPersistentTrackID_Invalid)
                }
                let aRange = try await at.load(.timeRange)
                let aDur = CMTimeMinimum(clipDur, aRange.duration)
                try compA?.insertTimeRange(CMTimeRange(start: .zero, duration: aDur), of: at, at: cursor)
            }
            cursor = CMTimeAdd(cursor, clipDur)
        }

        guard let session = AVAssetExportSession(asset: comp, presetName: AVAssetExportPresetPassthrough) else {
            throw NSError(domain: "stitch", code: 3, userInfo: [NSLocalizedDescriptionKey: "Cannot create export session"])
        }
        session.shouldOptimizeForNetworkUse = true
        try await session.export(to: outURL, as: .mp4)
        print("Stitched \(clipPaths.count) scene clips (\(String(format: "%.2f", cursor.seconds))s) -> \(outURL.path)")
    } catch {
        fputs("Stitch error: \(error)\n", stderr)
        exitCode = 1
    }
    sema.signal()
}
sema.wait()
exit(exitCode)
