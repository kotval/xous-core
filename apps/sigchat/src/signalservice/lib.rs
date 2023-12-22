// this is a stub for a wrapped libsignal
// TODO: at least add a comment about what types get imported from this
// for now, we should have ProvisioningUuid

pub mod signalservice {
    include!(concat!(env!("OUT_DIR"), "/signalservice.rs"));
}


