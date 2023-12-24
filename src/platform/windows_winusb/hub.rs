use std::{
    alloc::{self, Layout},
    ffi::c_void,
    io::ErrorKind,
    mem,
    os::windows::prelude::OwnedHandle,
    ptr::{addr_of, null_mut},
    slice,
};

use log::{error, warn};
use windows_sys::Win32::{
    Devices::{
        Properties::DEVPKEY_Device_Address,
        Usb::{
            GUID_DEVINTERFACE_USB_HUB, IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION,
            IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX, USB_DESCRIPTOR_REQUEST,
            USB_DESCRIPTOR_REQUEST_0, USB_NODE_CONNECTION_INFORMATION_EX,
        },
    },
    Foundation::{GetLastError, ERROR_GEN_FAILURE, TRUE},
    System::IO::DeviceIoControl,
};

use crate::Error;

use super::{
    cfgmgr32::DevInst,
    util::{create_file, raw_handle},
};

/// Safe wrapper around hub ioctls used to get descriptors for child devices.
pub struct HubHandle(OwnedHandle);

impl HubHandle {
    pub fn by_devinst(devinst: DevInst) -> Option<HubHandle> {
        let paths = devinst.interfaces(GUID_DEVINTERFACE_USB_HUB);
        let Some(path) = paths.iter().next() else {
            error!("Failed to find hub interface");
            return None;
        };

        match create_file(path) {
            Ok(f) => Some(HubHandle(f)),
            Err(e) => {
                error!("Failed to open hub: {e}");
                None
            }
        }
    }

    pub fn get_node_connection_info(
        &self,
        port_number: u32,
    ) -> Result<USB_NODE_CONNECTION_INFORMATION_EX, Error> {
        unsafe {
            let mut info: USB_NODE_CONNECTION_INFORMATION_EX = mem::zeroed();
            info.ConnectionIndex = port_number;
            let mut bytes_returned: u32 = 0;
            let r = DeviceIoControl(
                raw_handle(&self.0),
                IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX,
                &info as *const _ as *const c_void,
                mem::size_of_val(&info) as u32,
                &mut info as *mut _ as *mut c_void,
                mem::size_of_val(&info) as u32,
                &mut bytes_returned,
                null_mut(),
            );

            if r == TRUE {
                Ok(info)
            } else {
                let err = Error::last_os_error();
                error!("Hub DeviceIoControl failed: {err:?}");
                Err(err)
            }
        }
    }

    pub fn get_descriptor(
        &self,
        port_number: u32,
        descriptor_type: u8,
        descriptor_index: u8,
        language_id: u16,
    ) -> Result<Vec<u8>, Error> {
        // Experimentally determined on Windows 10 19045.3803 that this fails
        // with ERROR_INVALID_PARAMETER for non-cached descriptors when
        // requesting length greater than 4095.
        let length = 4095;

        unsafe {
            let layout = Layout::from_size_align(
                mem::size_of::<USB_DESCRIPTOR_REQUEST>() + length,
                mem::align_of::<USB_DESCRIPTOR_REQUEST>(),
            )
            .unwrap();

            let req = alloc::alloc(layout).cast::<USB_DESCRIPTOR_REQUEST>();

            req.write(USB_DESCRIPTOR_REQUEST {
                ConnectionIndex: port_number,
                SetupPacket: USB_DESCRIPTOR_REQUEST_0 {
                    bmRequest: 0x80,
                    bRequest: 0x06,
                    wValue: ((descriptor_type as u16) << 8) | descriptor_index as u16,
                    wIndex: language_id,
                    wLength: length as u16,
                },
                Data: [0],
            });

            let mut bytes_returned: u32 = 0;
            let r = DeviceIoControl(
                raw_handle(&self.0),
                IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION,
                req as *const c_void,
                layout.size() as u32,
                req as *mut c_void,
                layout.size() as u32,
                &mut bytes_returned,
                null_mut(),
            );

            let res = if r == TRUE {
                let start = addr_of!((*req).Data[0]);
                let end = (req as *mut u8).offset(bytes_returned as isize);
                let len = end.offset_from(start) as usize;
                let vec = slice::from_raw_parts(start, len).to_owned();
                Ok(vec)
            } else {
                let err = GetLastError();
                warn!("IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION failed: type={descriptor_type} index={descriptor_index} error={err:?}");
                Err(match err {
                    ERROR_GEN_FAILURE => Error::new(
                        ErrorKind::Other,
                        "Descriptor request failed. Device might be suspended.",
                    ),
                    _ => Error::from_raw_os_error(err as i32),
                })
            };

            alloc::dealloc(req as *mut _, layout);

            res
        }
    }
}

pub struct HubPort {
    hub_handle: HubHandle,
    port_number: u32,
}

impl HubPort {
    pub fn by_child_devinst(devinst: DevInst) -> Result<HubPort, Error> {
        let parent_hub = devinst
            .parent()
            .ok_or_else(|| Error::new(ErrorKind::Other, "failed to find parent hub"))?;
        let hub_handle = HubHandle::by_devinst(parent_hub)
            .ok_or_else(|| Error::new(ErrorKind::Other, "failed to open parent hub"))?;
        let Some(port_number) = devinst.get_property::<u32>(DEVPKEY_Device_Address) else {
            return Err(Error::new(
                ErrorKind::NotConnected,
                "Could not find hub port number",
            ));
        };

        Ok(HubPort {
            hub_handle,
            port_number,
        })
    }

    pub fn get_node_connection_info(&self) -> Result<USB_NODE_CONNECTION_INFORMATION_EX, Error> {
        self.hub_handle.get_node_connection_info(self.port_number)
    }

    pub fn get_descriptor(
        &self,
        descriptor_type: u8,
        descriptor_index: u8,
        language_id: u16,
    ) -> Result<Vec<u8>, Error> {
        self.hub_handle.get_descriptor(
            self.port_number,
            descriptor_type,
            descriptor_index,
            language_id,
        )
    }
}
