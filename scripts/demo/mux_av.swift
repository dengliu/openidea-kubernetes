import Foundation
import AVFoundation
import CoreMedia

guard CommandLine.arguments.count >= 4 else {
    fputs("Usage: mux_av <video.mp4> <audio.m4a> <output.mp4>\n", stderr)
    exit(1)
}

let videoURL = URL(fileURLWithPath: CommandLine.arguments[1])
let audioURL = URL(fileURLWithPath: CommandLine.arguments[2])
let outURL = URL(fileURLWithPath: CommandLine.arguments[3])
try? FileManager.default.removeItem(at: outURL)

let sema = DispatchSemaphore(value: 0)
var exitCode: Int32 = 0

Task {
    do {
        let vAsset = AVURLAsset(url: videoURL)
        let aAsset = AVURLAsset(url: audioURL)
        let vDur = try await vAsset.load(.duration)
        let aDur = try await aAsset.load(.duration)
        let vTracks = try await vAsset.loadTracks(withMediaType: .video)
        let aTracks = try await aAsset.loadTracks(withMediaType: .audio)

        let comp = AVMutableComposition()
        if let vTrack = vTracks.first,
           let compV = comp.addMutableTrack(withMediaType: .video, preferredTrackID: kCMPersistentTrackID_Invalid) {
            try compV.insertTimeRange(CMTimeRange(start: .zero, duration: vDur), of: vTrack, at: .zero)
        }
        if let aTrack = aTracks.first,
           let compA = comp.addMutableTrack(withMediaType: .audio, preferredTrackID: kCMPersistentTrackID_Invalid) {
            let muxDur = CMTimeMinimum(vDur, aDur)
            try compA.insertTimeRange(CMTimeRange(start: .zero, duration: muxDur), of: aTrack, at: .zero)
        }

        guard let session = AVAssetExportSession(asset: comp, presetName: AVAssetExportPresetPassthrough) else {
            throw NSError(domain: "mux", code: 1, userInfo: [NSLocalizedDescriptionKey: "Cannot create export session"])
        }
        session.shouldOptimizeForNetworkUse = true
        try await session.export(to: outURL, as: .mp4)
        print("Muxed video (\(String(format: "%.2f", vDur.seconds))s) + audio (\(String(format: "%.2f", aDur.seconds))s) -> \(outURL.path)")
    } catch {
        fputs("Mux error: \(error)\n", stderr)
        exitCode = 1
    }
    sema.signal()
}
sema.wait()
exit(exitCode)
