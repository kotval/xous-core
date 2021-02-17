#![cfg_attr(target_os = "none", no_std)]

pub mod api;
use api::*;

use xous::ipc::*;
use core::fmt::Write;
use graphics_server::api::{TextOp, TextView};

use graphics_server::api::{Rectangle, Point, Gid, TextBounds};
use log::{error, info};

use rkyv::Write;

/// this "posts" a textview -- it's not a "draw" as the update is neither guaranteed nor instantaneous
/// the GAM first has to check that the textview is allowed to be updated, and then it will decide when
/// the actual screen update is allowed
pub fn post_textview(gam_cid: xous::CID, tv: &mut TextView) -> Result<(), xous::Error> {
    // for testing only
    tv.set_op(TextOp::Render);
    tv.cursor.pt.x = 37;
    tv.cursor.pt.y = 5;

    info!("tv before lend: {:?}", tv);
    let mut rkyv_tv = api::Opcode::RenderTextView(tv);
    let mut writer = rkyv::ArchiveBuffer::new(xous::XousBuffer::new(4096));
    let pos = writer.archive(&rkyv_tv).expect("couldn't archive textview");
    let mut xous_buffer = writer.into_inner();

    xous_buffer.lend_mut(gam_cid, pos as u32).expect("RenderTextView operation failure");

    let returned = unsafe { rkyv::archived_value::<api::Opcode>(xous_buffer.as_ref(), pos)};
    match returned {
        rkyv::Archived::<api::Opcode>::TextViewResult(result) => {
            tv.set_bounds_computed(result.bounds_computed);
            tv.cursor = result.cursor;
        },
        _ => panic!("post_textview got a return value from the server that isn't expected or handled")
    }

    tv.set_op(TextOp::Nop);
    Ok(())
}