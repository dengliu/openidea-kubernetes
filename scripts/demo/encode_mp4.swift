import Foundation
import AVFoundation
import CoreGraphics
import CoreVideo
import CoreMedia
import ImageIO

struct FrameEntry: Decodable {
    let file: String
    let durationMs: Int
}

guard CommandLine.arguments.count >= 3 else {
    fputs("Usage: encode_mp4 <manifest.json> <output.mp4>\n", stderr)
    exit(1)
}

let manifestPath = CommandLine.arguments[1]
let outputPath = CommandLine.arguments[2]
let width = 1920
let height = 1080

let manifestData = try Data(contentsOf: URL(fileURLWithPath: manifestPath))
let frames = try JSONDecoder().decode([FrameEntry].self, from: manifestData).filter { $0.durationMs > 0 }
guard !frames.isEmpty else {
    fputs("No frames in manifest\n", stderr)
    exit(1)
}

let outputURL = URL(fileURLWithPath: outputPath)
try? FileManager.default.removeItem(at: outputURL)

let writer = try AVAssetWriter(outputURL: outputURL, fileType: .mp4)
writer.shouldOptimizeForNetworkUse = true

let compressionProps: [String: Any] = [
    AVVideoAverageBitRateKey: 12_000_000,
    AVVideoMaxKeyFrameIntervalKey: 30,
    AVVideoProfileLevelKey: AVVideoProfileLevelH264HighAutoLevel,
    AVVideoAllowFrameReorderingKey: true
]

let videoSettings: [String: Any] = [
    AVVideoCodecKey: AVVideoCodecType.h264,
    AVVideoWidthKey: width,
    AVVideoHeightKey: height,
    AVVideoCompressionPropertiesKey: compressionProps
]

let writerInput = AVAssetWriterInput(mediaType: .video, outputSettings: videoSettings)
writerInput.expectsMediaDataInRealTime = false

let pixelBufferAttrs: [String: Any] = [
    kCVPixelBufferPixelFormatTypeKey as String: Int(kCVPixelFormatType_32ARGB),
    kCVPixelBufferWidthKey as String: width,
    kCVPixelBufferHeightKey as String: height,
    kCVPixelBufferCGImageCompatibilityKey as String: true,
    kCVPixelBufferCGBitmapContextCompatibilityKey as String: true
]

let adaptor = AVAssetWriterInputPixelBufferAdaptor(
    assetWriterInput: writerInput,
    sourcePixelBufferAttributes: pixelBufferAttrs
)

writer.add(writerInput)
guard writer.startWriting() else {
    fputs("Failed to start writing: \(String(describing: writer.error))\n", stderr)
    exit(1)
}
writer.startSession(atSourceTime: .zero)

func makePixelBuffer(from imagePath: String, pool: CVPixelBufferPool) -> CVPixelBuffer? {
    guard let src = CGImageSourceCreateWithURL(URL(fileURLWithPath: imagePath) as CFURL, nil),
          let cgImage = CGImageSourceCreateImageAtIndex(src, 0, nil) else { return nil }
    var pbOut: CVPixelBuffer?
    guard CVPixelBufferPoolCreatePixelBuffer(kCFAllocatorDefault, pool, &pbOut) == kCVReturnSuccess,
          let pb = pbOut else { return nil }
    CVPixelBufferLockBaseAddress(pb, [])
    defer { CVPixelBufferUnlockBaseAddress(pb, []) }
    guard let base = CVPixelBufferGetBaseAddress(pb) else { return nil }
    let bytesPerRow = CVPixelBufferGetBytesPerRow(pb)
    guard let ctx = CGContext(
        data: base,
        width: width,
        height: height,
        bitsPerComponent: 8,
        bytesPerRow: bytesPerRow,
        space: CGColorSpaceCreateDeviceRGB(),
        bitmapInfo: CGImageAlphaInfo.noneSkipFirst.rawValue | CGBitmapInfo.byteOrder32Big.rawValue
    ) else { return nil }
    ctx.interpolationQuality = .high
    ctx.draw(cgImage, in: CGRect(x: 0, y: 0, width: width, height: height))
    return pb
}

var currentMs: Int64 = 0
let stepMs: Int64 = 33

for (idx, entry) in frames.enumerated() {
    guard let pool = adaptor.pixelBufferPool,
          let pb = makePixelBuffer(from: entry.file, pool: pool) else {
        fputs("Failed to load frame \(entry.file)\n", stderr)
        exit(1)
    }
    let dur = max(Int64(entry.durationMs), stepMs)
    let endMs = currentMs + dur
    var t = currentMs
    while t < endMs {
        while !writerInput.isReadyForMoreMediaData { Thread.sleep(forTimeInterval: 0.002) }
        if !adaptor.append(pb, withPresentationTime: CMTime(value: t, timescale: 1000)) { exit(1) }
        t += stepMs
    }
    currentMs = t
}

writerInput.markAsFinished()
writer.endSession(atSourceTime: CMTime(value: currentMs, timescale: 1000))
let sema = DispatchSemaphore(value: 0)
writer.finishWriting { sema.signal() }
sema.wait()
guard writer.status == .completed else {
    fputs("Writer failed: \(String(describing: writer.error))\n", stderr)
    exit(1)
}
print("Encoded \(frames.count) keyframes (\(Double(currentMs) / 1000.0)s) -> \(outputPath)")
