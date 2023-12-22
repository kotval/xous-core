use std::io::Result;
fn main() -> Result<()> {
    prost_build::compile_protos(
        &[
            "src/protos/Provisioning.proto",
            "src/protos/SignalService.proto",
            "src/protos/Groups.proto",
            "src/protos/StickerResources.proto",
            "src/protos/DeviceName.proto",
            "src/protos/UnidentifiedDelivery.proto",
            "src/protos/WebSocketResources.proto",
        ],
        &["src/signalservice"],
    )?;
    Ok(())
}
