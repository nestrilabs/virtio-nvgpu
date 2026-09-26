# What the application pass (probes/apps.sh) opens, at /opt/nvgpu/apps:
# the pieces under ../apps (test pages, a Godot project, a QML scene, an
# Electron app, Blender scripts), and media made here with ffmpeg so the guest
# needs no network: 1080p test clips in H.264, HEVC, AV1 and VP9 (next to the
# page that plays them, www/), a PNG and an SVG to open.
{ pkgs, appsSrc }:
pkgs.runCommand "nvgpu-apps-data"
  {
    nativeBuildInputs = [ pkgs.ffmpeg-full ];
  }
  ''
    mkdir -p $out
    cp -r ${appsSrc}/. $out/
    chmod -R u+w $out
    cd $out/www
    src="-f lavfi -i testsrc2=size=1920x1080:rate=30 -t 10 -pix_fmt yuv420p"
    ffmpeg -hide_banner -loglevel error $src -c:v libx264 -preset veryfast -b:v 8M h264.mp4
    ffmpeg -hide_banner -loglevel error $src -c:v libx265 -preset veryfast -b:v 8M -tag:v hvc1 hevc.mp4
    ffmpeg -hide_banner -loglevel error $src -c:v libsvtav1 -preset 10 -b:v 6M av1.mp4
    ffmpeg -hide_banner -loglevel error $src -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -b:v 6M vp9.webm
    ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=1920x1080 -frames:v 1 ../frame.png
    cat > ../drawing.svg <<'SVG'
    <svg xmlns="http://www.w3.org/2000/svg" width="800" height="600" viewBox="0 0 800 600">
      <rect width="800" height="600" fill="#1d3557"/>
      <circle cx="400" cy="300" r="180" fill="#e63946" stroke="#f1faee" stroke-width="12"/>
      <text x="400" y="315" font-family="DejaVu Sans" font-size="48" fill="#f1faee" text-anchor="middle">virtio-nvgpu</text>
    </svg>
    SVG
    ls -l $out/www
  ''
