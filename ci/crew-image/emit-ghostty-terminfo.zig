// Copied into Ghostty's src/ directory so the relative import uses the
// terminfo definition from the pinned Ghostty checkout.
const std = @import("std");

pub fn main(init: std.process.Init) !void {
    var buffer: [1024]u8 = undefined;
    var stdout_writer = std.Io.File.stdout().writerStreaming(init.io, &buffer);
    try @import("terminfo/ghostty.zig").ghostty.encode(&stdout_writer.interface);
    try stdout_writer.end();
}
