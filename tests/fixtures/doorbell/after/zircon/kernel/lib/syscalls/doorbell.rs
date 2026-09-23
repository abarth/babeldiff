// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// Synthetic syscalls used to test babeldiff. They are not part of Zircon.

use crate::object::dispatcher::Dispatcher;
use crate::object::doorbell_dispatcher::DoorbellDispatcher;
use crate::user_copy::UserOutPtr;
use zx_status::Status;
use zx_types::{HandleValue, ZX_RIGHT_SIGNAL};

const LOCAL_TRACE: bool = false;

pub fn sys_doorbell_ring(
    handle: HandleValue,
    tone: u32,
    out_count: UserOutPtr<u64>,
) -> Result<(), Status> {
    ltracef!("handle {:?} tone {}\n", handle, tone);

    let doorbell = Dispatcher::get_with_rights::<DoorbellDispatcher>(handle, ZX_RIGHT_SIGNAL)?;
    doorbell.ring(tone)?;
    if !out_count.is_null() {
        out_count.write(doorbell.ring_count())?;
    }
    Ok(())
}
