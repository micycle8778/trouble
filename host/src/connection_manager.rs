use core::cell::RefCell;
use core::future::poll_fn;
use core::task::{Context, Poll};

use bt_hci::param::{AddrKind, BdAddr, ConnHandle, DisconnectReason, LeConnRole};
use embassy_sync::waitqueue::WakerRegistration;

use crate::Error;

struct State<'d> {
    connections: &'d mut [ConnectionStorage],
    accept_waker: WakerRegistration,
    disconnect_waker: WakerRegistration,
    default_link_credits: usize,
}

impl<'d> State<'d> {
    fn print(&self) {
        for (idx, storage) in self.connections.iter().enumerate() {
            if storage.state != ConnectionState::Disconnected {
                debug!("[link][idx = {}] state = {:?}", idx, storage);
            }
        }
    }
}

pub(crate) struct ConnectionManager<'d> {
    state: RefCell<State<'d>>,
}

impl<'d> ConnectionManager<'d> {
    pub(crate) fn new(connections: &'d mut [ConnectionStorage]) -> Self {
        Self {
            state: RefCell::new(State {
                connections,
                accept_waker: WakerRegistration::new(),
                disconnect_waker: WakerRegistration::new(),
                default_link_credits: 0,
            }),
        }
    }

    pub(crate) fn role(&self, index: u8) -> LeConnRole {
        self.with_mut(|state| {
            let state = &mut state.connections[index as usize];
            state.role.unwrap()
        })
    }

    pub(crate) fn handle(&self, index: u8) -> ConnHandle {
        self.with_mut(|state| {
            let state = &mut state.connections[index as usize];
            state.handle.unwrap()
        })
    }

    pub(crate) fn peer_address(&self, index: u8) -> BdAddr {
        self.with_mut(|state| {
            let state = &mut state.connections[index as usize];
            state.peer_addr.unwrap()
        })
    }

    pub(crate) fn request_disconnect(&self, index: u8, reason: DisconnectReason) {
        self.with_mut(|state| {
            let state = &mut state.connections[index as usize];
            if state.state == ConnectionState::Connected {
                trace!(
                    "[host] requesting {} (handle {:?}) to be disconnected",
                    index,
                    state.handle.unwrap()
                );
                state.state = ConnectionState::DisconnectRequest(reason);
            }
        })
    }

    pub(crate) fn poll_disconnecting<'m>(
        &'m self,
        cx: Option<&mut Context<'_>>,
    ) -> Poll<DisconnectRequestIter<'m, 'd>> {
        let mut state = self.state.borrow_mut();
        if let Some(cx) = cx {
            state.disconnect_waker.register(cx.waker());
        }
        for storage in state.connections.iter() {
            if let ConnectionState::DisconnectRequest(_) = storage.state {
                return Poll::Ready(DisconnectRequestIter {
                    next: 0,
                    state: &self.state,
                });
            }
        }
        Poll::Pending
    }

    pub(crate) fn is_connected(&self, h: ConnHandle) -> bool {
        let mut state = self.state.borrow_mut();
        for storage in state.connections.iter_mut() {
            match (storage.handle, &storage.state) {
                (Some(handle), ConnectionState::Connected) if handle == h => {
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    pub(crate) fn disconnected(&self, h: ConnHandle) -> Result<(), Error> {
        let mut state = self.state.borrow_mut();
        for storage in state.connections.iter_mut() {
            if let Some(handle) = storage.handle {
                if handle == h {
                    storage.state = ConnectionState::Disconnected;
                    state.disconnect_waker.wake();
                    return Ok(());
                }
            }
        }
        trace!("[link][disconnect] connection handle {:?} not found", h);
        Err(Error::NotFound)
    }

    pub(crate) fn connect(
        &self,
        handle: ConnHandle,
        peer_addr_kind: AddrKind,
        peer_addr: BdAddr,
        role: LeConnRole,
    ) -> Result<(), Error> {
        let mut state = self.state.borrow_mut();
        let default_credits = state.default_link_credits;
        for storage in state.connections.iter_mut() {
            if ConnectionState::Disconnected == storage.state && storage.refcount == 0 {
                storage.state = ConnectionState::Connecting;
                storage.link_credits = default_credits;
                storage.handle.replace(handle);
                storage.peer_addr_kind.replace(peer_addr_kind);
                storage.peer_addr.replace(peer_addr);
                storage.role.replace(role);
                state.accept_waker.wake();
                return Ok(());
            }
        }
        trace!("[link][connect] no available slot found for handle {:?}", handle);
        Err(Error::NotFound)
    }

    pub(crate) fn poll_accept(
        &self,
        role: LeConnRole,
        peers: &[(AddrKind, &BdAddr)],
        cx: Option<&mut Context<'_>>,
    ) -> Poll<u8> {
        let mut state = self.state.borrow_mut();
        if let Some(cx) = cx {
            state.accept_waker.register(cx.waker());
        }
        for (idx, storage) in state.connections.iter_mut().enumerate() {
            if let ConnectionState::Connecting = storage.state {
                let handle = storage.handle.unwrap();
                let r = storage.role.unwrap();
                if r == role {
                    if !peers.is_empty() {
                        for peer in peers.iter() {
                            if storage.peer_addr_kind.unwrap() == peer.0 && &storage.peer_addr.unwrap() == peer.1 {
                                storage.state = ConnectionState::Connected;
                                trace!(
                                    "[link][poll_accept] connection handle {:?} in role {} accepted",
                                    handle,
                                    role
                                );
                                return Poll::Ready(idx as u8);
                            }
                        }
                    } else {
                        storage.state = ConnectionState::Connected;
                        trace!(
                            "[link][poll_accept] connection handle {:?} in role {} accepted",
                            handle,
                            role
                        );
                        return Poll::Ready(idx as u8);
                    }
                }
            }
        }
        Poll::Pending
    }

    fn with_mut<F: FnOnce(&mut State<'d>) -> R, R>(&self, f: F) -> R {
        let mut state = self.state.borrow_mut();
        f(&mut state)
    }

    pub(crate) fn log_status(&self) {
        let state = self.state.borrow();
        state.print();
    }

    pub(crate) fn inc_ref(&self, index: u8) {
        self.with_mut(|state| {
            let state = &mut state.connections[index as usize];
            state.refcount = unwrap!(
                state.refcount.checked_add(1),
                "Too many references to the same connection"
            );
        });
    }

    pub(crate) fn dec_ref(&self, index: u8) {
        self.with_mut(|state| {
            let state = &mut state.connections[index as usize];
            state.refcount = unwrap!(
                state.refcount.checked_sub(1),
                "bug: dropping a connection with refcount 0"
            );
            if state.refcount == 0 && state.state == ConnectionState::Connected {
                state.state = ConnectionState::Disconnecting(DisconnectReason::RemoteUserTerminatedConn);
            }
        });
    }

    pub(crate) async fn accept(&self, role: LeConnRole, peers: &[(AddrKind, &BdAddr)]) -> u8 {
        poll_fn(move |cx| self.poll_accept(role, peers, Some(cx))).await
    }

    pub(crate) fn set_link_credits(&self, credits: usize) {
        let mut state = self.state.borrow_mut();
        state.default_link_credits = credits;
        for storage in state.connections.iter_mut() {
            storage.link_credits = credits;
        }
    }

    pub(crate) fn confirm_sent(&self, handle: ConnHandle, packets: usize) -> Result<(), Error> {
        let mut state = self.state.borrow_mut();
        for storage in state.connections.iter_mut() {
            match storage.state {
                ConnectionState::Connected if handle == storage.handle.unwrap() => {
                    storage.link_credits += packets;
                    storage.link_credit_waker.wake();
                    return Ok(());
                }
                _ => {}
            }
        }
        trace!("[link][confirm_sent] connection {:?} not found", handle);
        Err(Error::NotFound)
    }

    pub(crate) fn poll_request_to_send(
        &self,
        handle: ConnHandle,
        packets: usize,
        cx: Option<&mut Context<'_>>,
    ) -> Poll<Result<PacketGrant<'_, 'd>, Error>> {
        let mut state = self.state.borrow_mut();
        for storage in state.connections.iter_mut() {
            match storage.state {
                ConnectionState::Connected if storage.handle.unwrap() == handle => {
                    if packets <= storage.link_credits {
                        storage.link_credits -= packets;

                        return Poll::Ready(Ok(PacketGrant::new(&self.state, handle, packets)));
                    } else {
                        if let Some(cx) = cx {
                            storage.link_credit_waker.register(cx.waker());
                        }
                        debug!(
                            "[link][poll_request_to_send][conn = {}] requested {} available {}",
                            handle.raw(),
                            packets,
                            storage.link_credits
                        );
                        return Poll::Pending;
                    }
                }
                _ => {}
            }
        }
        trace!("[link][pool_request_to_send] connection {:?} not found", handle);
        Poll::Ready(Err(Error::NotFound))
    }
}

pub(crate) trait DynamicConnectionManager {
    fn role(&self, index: u8) -> LeConnRole;
    fn handle(&self, index: u8) -> ConnHandle;
    fn peer_address(&self, index: u8) -> BdAddr;
    fn inc_ref(&self, index: u8);
    fn dec_ref(&self, index: u8);
    fn disconnect(&self, index: u8, reason: DisconnectReason);
    fn get_att_mtu(&self, conn: ConnHandle) -> u16;
    fn exchange_att_mtu(&self, conn: ConnHandle, mtu: u16) -> u16;
}

impl<'d> DynamicConnectionManager for ConnectionManager<'d> {
    fn role(&self, index: u8) -> LeConnRole {
        ConnectionManager::role(self, index)
    }
    fn handle(&self, index: u8) -> ConnHandle {
        ConnectionManager::handle(self, index)
    }
    fn peer_address(&self, index: u8) -> BdAddr {
        ConnectionManager::peer_address(self, index)
    }
    fn inc_ref(&self, index: u8) {
        ConnectionManager::inc_ref(self, index)
    }
    fn dec_ref(&self, index: u8) {
        ConnectionManager::dec_ref(self, index)
    }
    fn disconnect(&self, index: u8, reason: DisconnectReason) {
        ConnectionManager::request_disconnect(self, index, reason)
    }
    fn get_att_mtu(&self, conn: ConnHandle) -> u16 {
        let mut state = self.state.borrow_mut();
        for storage in state.connections.iter_mut() {
            match storage.state {
                ConnectionState::Connected if storage.handle.unwrap() == conn => {
                    return storage.att_mtu;
                }
                _ => {}
            }
        }
        23 // Minimum value
    }
    fn exchange_att_mtu(&self, conn: ConnHandle, mtu: u16) -> u16 {
        let mut state = self.state.borrow_mut();
        for storage in state.connections.iter_mut() {
            match storage.state {
                ConnectionState::Connected if storage.handle.unwrap() == conn => {
                    storage.att_mtu = storage.att_mtu.min(mtu);
                    return storage.att_mtu;
                }
                _ => {}
            }
        }
        mtu
    }
}

pub struct DisconnectRequest<'a, 'd> {
    index: usize,
    handle: ConnHandle,
    reason: DisconnectReason,
    state: &'a RefCell<State<'d>>,
}

impl<'a, 'd> DisconnectRequest<'a, 'd> {
    pub fn handle(&self) -> ConnHandle {
        self.handle
    }

    pub fn reason(&self) -> DisconnectReason {
        self.reason
    }

    pub fn confirm(self) {
        let mut state = self.state.borrow_mut();
        state.connections[self.index].state = ConnectionState::Disconnecting(self.reason);
    }
}

pub struct DisconnectRequestIter<'a, 'd> {
    state: &'a RefCell<State<'d>>,
    next: usize,
}

impl<'a, 'd> Iterator for DisconnectRequestIter<'a, 'd> {
    type Item = DisconnectRequest<'a, 'd>;
    fn next(&mut self) -> Option<Self::Item> {
        let state = self.state.borrow();
        for idx in self.next..state.connections.len() {
            if let ConnectionState::DisconnectRequest(reason) = state.connections[idx].state {
                self.next = idx + 1;
                return state.connections[idx].handle.map(|handle| DisconnectRequest {
                    index: idx,
                    handle,
                    reason,
                    state: self.state,
                });
            }
        }
        self.next = state.connections.len();
        None
    }
}

#[derive(Debug)]
pub struct ConnectionStorage {
    pub state: ConnectionState,
    pub handle: Option<ConnHandle>,
    pub role: Option<LeConnRole>,
    pub peer_addr_kind: Option<AddrKind>,
    pub peer_addr: Option<BdAddr>,
    pub att_mtu: u16,
    pub link_credits: usize,
    pub link_credit_waker: WakerRegistration,
    pub refcount: u8,
}

impl ConnectionStorage {
    pub(crate) const DISCONNECTED: ConnectionStorage = ConnectionStorage {
        state: ConnectionState::Disconnected,
        handle: None,
        role: None,
        peer_addr_kind: None,
        peer_addr: None,
        att_mtu: 23,
        link_credits: 0,
        link_credit_waker: WakerRegistration::new(),
        refcount: 0,
    };
}

#[cfg(feature = "defmt")]
impl defmt::Format for ConnectionStorage {
    fn format(&self, f: defmt::Formatter<'_>) {
        defmt::write!(
            f,
            "state = {}, conn = {}, credits = {}, role = {}, peer = {:?}",
            self.state,
            self.handle,
            self.link_credits,
            self.role,
            self.peer_addr,
        );
    }
}

#[derive(Debug, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ConnectionState {
    DisconnectRequest(DisconnectReason),
    Disconnecting(DisconnectReason),
    Disconnected,
    Connecting,
    Connected,
}

pub struct PacketGrant<'a, 'd> {
    state: &'a RefCell<State<'d>>,
    handle: ConnHandle,
    packets: usize,
}

impl<'a, 'd> PacketGrant<'a, 'd> {
    fn new(state: &'a RefCell<State<'d>>, handle: ConnHandle, packets: usize) -> Self {
        Self { state, handle, packets }
    }

    pub(crate) fn confirm(&mut self, sent: usize) {
        self.packets = self.packets.saturating_sub(sent);
    }
}

impl<'a, 'd> Drop for PacketGrant<'a, 'd> {
    fn drop(&mut self) {
        if self.packets > 0 {
            let mut state = self.state.borrow_mut();
            for storage in state.connections.iter_mut() {
                match storage.state {
                    ConnectionState::Connected if self.handle == storage.handle.unwrap() => {
                        storage.link_credits += self.packets;
                        storage.link_credit_waker.wake();
                        break;
                    }
                    _ => {}
                }
            }
            // make it an assert?
            warn!("[link] connection {:?} not found", self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR_1: [u8; 6] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
    const ADDR_2: [u8; 6] = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];

    #[test]
    fn peripheral_connection_established() {
        let mut storage = [ConnectionStorage::DISCONNECTED; 3];
        let mgr = ConnectionManager::new(&mut storage[..]);

        assert_eq!(Poll::Pending, mgr.poll_accept(LeConnRole::Peripheral, &[], None));

        unwrap!(mgr.connect(
            ConnHandle::new(0),
            AddrKind::RANDOM,
            BdAddr::new(ADDR_1),
            LeConnRole::Peripheral
        ));

        let Poll::Ready(index) = mgr.poll_accept(LeConnRole::Peripheral, &[], None) else {
            panic!("expected connection to be accepted");
        };
        assert_eq!(mgr.role(index), LeConnRole::Peripheral);
        assert_eq!(mgr.peer_address(index), BdAddr::new(ADDR_1));

        mgr.request_disconnect(index, DisconnectReason::RemoteUserTerminatedConn);
    }

    #[test]
    fn central_connection_established() {
        let mut storage = [ConnectionStorage::DISCONNECTED; 3];
        let mgr = ConnectionManager::new(&mut storage[..]);

        assert_eq!(Poll::Pending, mgr.poll_accept(LeConnRole::Central, &[], None));

        unwrap!(mgr.connect(
            ConnHandle::new(0),
            AddrKind::RANDOM,
            BdAddr::new(ADDR_2),
            LeConnRole::Central
        ));

        let Poll::Ready(index) = mgr.poll_accept(LeConnRole::Central, &[], None) else {
            panic!("expected connection to be accepted");
        };
        assert_eq!(mgr.role(index), LeConnRole::Central);
        assert_eq!(mgr.peer_address(index), BdAddr::new(ADDR_2));
    }

    #[test]
    fn controller_disconnects_before_host() {
        let mut storage = [ConnectionStorage::DISCONNECTED; 3];
        let mgr = ConnectionManager::new(&mut storage[..]);

        unwrap!(mgr.connect(
            ConnHandle::new(3),
            AddrKind::RANDOM,
            BdAddr::new(ADDR_1),
            LeConnRole::Central
        ));

        unwrap!(mgr.connect(
            ConnHandle::new(2),
            AddrKind::RANDOM,
            BdAddr::new(ADDR_2),
            LeConnRole::Peripheral
        ));

        let Poll::Ready(central) = mgr.poll_accept(LeConnRole::Central, &[], None) else {
            panic!("expected connection to be accepted");
        };

        let Poll::Ready(peripheral) = mgr.poll_accept(LeConnRole::Peripheral, &[], None) else {
            panic!("expected connection to be accepted");
        };

        assert_eq!(ConnHandle::new(3), mgr.handle(central));
        assert_eq!(ConnHandle::new(2), mgr.handle(peripheral));

        // Disconnect request from us
        mgr.request_disconnect(peripheral, DisconnectReason::RemoteUserTerminatedConn);

        // Polling should return the disconnecting handle
        let Poll::Ready(mut it) = mgr.poll_disconnecting(None) else {
            panic!("expected connection to be accepted");
        };

        let next = unwrap!(it.next());
        assert!(it.next().is_none());

        // Disconnection event from host arrives before we confirm
        unwrap!(mgr.disconnected(ConnHandle::new(2)));

        // This should be a noop
        next.confirm();

        // Polling should not return anything
        assert!(mgr.poll_disconnecting(None).is_pending());
    }

    #[test]
    fn controller_disconnects_after_host() {
        let mut storage = [ConnectionStorage::DISCONNECTED; 3];
        let mgr = ConnectionManager::new(&mut storage[..]);

        unwrap!(mgr.connect(
            ConnHandle::new(3),
            AddrKind::RANDOM,
            BdAddr::new(ADDR_1),
            LeConnRole::Central
        ));

        unwrap!(mgr.connect(
            ConnHandle::new(2),
            AddrKind::RANDOM,
            BdAddr::new(ADDR_2),
            LeConnRole::Peripheral
        ));

        let Poll::Ready(central) = mgr.poll_accept(LeConnRole::Central, &[], None) else {
            panic!("expected connection to be accepted");
        };

        let Poll::Ready(peripheral) = mgr.poll_accept(LeConnRole::Peripheral, &[], None) else {
            panic!("expected connection to be accepted");
        };

        assert_eq!(ConnHandle::new(3), mgr.handle(central));
        assert_eq!(ConnHandle::new(2), mgr.handle(peripheral));

        // Disconnect request from us
        mgr.request_disconnect(peripheral, DisconnectReason::RemoteUserTerminatedConn);

        // Polling should return the disconnecting handle
        let Poll::Ready(mut it) = mgr.poll_disconnecting(None) else {
            panic!("expected connection to be accepted");
        };

        let next = unwrap!(it.next());
        assert!(it.next().is_none());

        // This should remove it from the list
        next.confirm();

        // Polling should not return anything
        assert!(mgr.poll_disconnecting(None).is_pending());

        // Disconnection event from host arrives before we confirm
        unwrap!(mgr.disconnected(ConnHandle::new(2)));

        // Polling should not return anything
        assert!(mgr.poll_disconnecting(None).is_pending());
    }
}
