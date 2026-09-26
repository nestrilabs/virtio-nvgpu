// A Qt Quick scene for the application pass (apps.sh qml, qmlvk): rotating
// rectangles, and the scene graph's API and frame rate on screen and in the log.
import QtQuick
import QtQuick.Window

Window {
    id: win
    width: 960; height: 600; visible: true
    title: "nvgpu-qml api=" + apiName
    color: "#1d2a44"
    property int frames: 0
    property string apiName: ["unknown", "software", "openvg", "opengl", "d3d11", "vulkan", "metal", "null", "d3d12"][GraphicsInfo.api] || ("" + GraphicsInfo.api)
    onFrameSwapped: frames++
    Timer {
        interval: 5000; running: true; repeat: true
        onTriggered: { console.log("NVGPU_QML api=" + win.apiName + " fps=" + (win.frames / 5)); win.frames = 0 }
    }
    Repeater {
        model: 48
        Rectangle {
            x: 60 + (index % 8) * 110; y: 90 + Math.floor(index / 8) * 80
            width: 70; height: 50; radius: 8
            color: Qt.hsla(index / 48, 0.7, 0.55, 1)
            RotationAnimation on rotation { from: 0; to: 360; duration: 1500 + index * 40; loops: Animation.Infinite }
        }
    }
    Text {
        x: 20; y: 20; color: "white"; font.pixelSize: 28
        text: "Qt Quick on virtio-nvgpu, scene graph API: " + win.apiName
    }
}
