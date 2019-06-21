// Copyright 2018 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

#[cfg(not(feature = "mock-network"))]
use routing::Client as Routing;
#[cfg(feature = "mock-network")]
use safe_core::client::NewFullId;
#[cfg(feature = "mock-network")]
use safe_core::MockRouting as Routing;

use crate::errors::AppError;
use crate::{AppContext, AppMsgTx};
use lru_cache::LruCache;
use routing::{Authority, FullId, XorName};
use rust_sodium::crypto::{box_, sign};
use safe_core::client::{
    setup_routing, spawn_routing_thread, ClientInner, IMMUT_DATA_CACHE_SIZE, REQUEST_TIMEOUT_SECS,
};
use safe_core::crypto::{shared_box, shared_secretbox, shared_sign};
use safe_core::ipc::BootstrapConfig;
use safe_core::{Client, ClientKeys, NetworkTx};
use safe_nd::{AppFullId, Message, MessageId, PublicKey, Request};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;
use std::time::Duration;
use tokio_core::reactor::Handle;

/// Client object used by safe_app.
pub struct AppClient {
    inner: Rc<RefCell<ClientInner<AppClient, AppContext>>>,
    app_inner: Rc<RefCell<AppInner>>,
}

impl AppClient {
    /// This is a getter-only Gateway function to the Maidsafe network. It will create an
    /// unregistered random client which can do a very limited set of operations, such as a
    /// Network-Get.
    pub(crate) fn unregistered(
        el_handle: Handle,
        core_tx: AppMsgTx,
        net_tx: NetworkTx,
        config: Option<BootstrapConfig>,
    ) -> Result<Self, AppError> {
        trace!("Creating unregistered client.");

        let (routing, routing_rx) = setup_routing(None, None, config)?;
        let joiner = spawn_routing_thread(routing_rx, core_tx.clone(), net_tx.clone());

        Ok(Self {
            inner: Rc::new(RefCell::new(ClientInner::new(
                el_handle,
                routing,
                HashMap::with_capacity(10),
                LruCache::new(IMMUT_DATA_CACHE_SIZE),
                Duration::from_secs(REQUEST_TIMEOUT_SECS),
                joiner,
                core_tx,
                net_tx,
            ))),
            app_inner: Rc::new(RefCell::new(AppInner::new(None, None, None, config))),
        })
    }

    /// This is a Gateway function to the Maidsafe network. This will help
    /// apps to authorise using an existing pair of keys.
    pub(crate) fn from_keys(
        keys: ClientKeys,
        owner: PublicKey,
        el_handle: Handle,
        core_tx: AppMsgTx,
        net_tx: NetworkTx,
        config: BootstrapConfig,
    ) -> Result<Self, AppError> {
        Self::from_keys_impl(keys, owner, el_handle, core_tx, net_tx, config, |routing| {
            routing
        })
    }

    /// Allows customising the mock Routing client before logging in using client keys.
    #[cfg(any(
        all(test, feature = "mock-network"),
        all(feature = "testing", feature = "mock-network")
    ))]
    pub(crate) fn from_keys_with_hook<F>(
        keys: ClientKeys,
        owner: PublicKey,
        el_handle: Handle,
        core_tx: AppMsgTx,
        net_tx: NetworkTx,
        config: BootstrapConfig,
        routing_wrapper_fn: F,
    ) -> Result<Self, AppError>
    where
        F: Fn(Routing) -> Routing,
    {
        Self::from_keys_impl(
            keys,
            owner,
            el_handle,
            core_tx,
            net_tx,
            config,
            routing_wrapper_fn,
        )
    }

    fn from_keys_impl<F>(
        keys: ClientKeys,
        owner: PublicKey,
        el_handle: Handle,
        core_tx: AppMsgTx,
        net_tx: NetworkTx,
        config: BootstrapConfig,
        routing_wrapper_fn: F,
    ) -> Result<Self, AppError>
    where
        F: Fn(Routing) -> Routing,
    {
        trace!("Attempting to log into an acc using client keys.");
        let (mut routing, routing_rx) = setup_routing(
            Some(keys.clone().into()),
            Some(NewFullId::App(AppFullId::with_keys(
                keys.bls_sk.clone(),
                owner,
            ))),
            Some(config),
        )?;
        routing = routing_wrapper_fn(routing);
        let joiner = spawn_routing_thread(routing_rx, core_tx.clone(), net_tx.clone());

        let cm_addr = Authority::ClientManager(XorName::from(owner));

        Ok(Self {
            inner: Rc::new(RefCell::new(ClientInner::new(
                el_handle,
                routing,
                HashMap::with_capacity(10),
                LruCache::new(IMMUT_DATA_CACHE_SIZE),
                Duration::from_secs(REQUEST_TIMEOUT_SECS),
                joiner,
                core_tx,
                net_tx,
            ))),
            app_inner: Rc::new(RefCell::new(AppInner::new(
                Some(keys),
                Some(owner),
                Some(cm_addr),
                Some(config),
            ))),
        })
    }
}

impl Client for AppClient {
    type MsgType = AppContext;

    fn full_id(&self) -> Option<FullId> {
        let app_inner = self.app_inner.borrow();
        app_inner.keys.clone().map(Into::into)
    }

    fn full_id_new(&self) -> Option<NewFullId> {
        Some(NewFullId::App(AppFullId::with_keys(
            self.secret_bls_key()?,
            self.owner_key()?,
        )))
    }

    fn config(&self) -> Option<BootstrapConfig> {
        let app_inner = self.app_inner.borrow();
        app_inner.config
    }

    fn cm_addr(&self) -> Option<Authority<XorName>> {
        let app_inner = self.app_inner.borrow();
        app_inner.cm_addr
    }

    fn inner(&self) -> Rc<RefCell<ClientInner<Self, Self::MsgType>>> {
        self.inner.clone()
    }

    fn public_signing_key(&self) -> Option<sign::PublicKey> {
        let app_inner = self.app_inner.borrow();
        Some(app_inner.keys.clone()?.sign_pk)
    }

    fn secret_signing_key(&self) -> Option<shared_sign::SecretKey> {
        let app_inner = self.app_inner.borrow();
        Some(app_inner.keys.clone()?.sign_sk)
    }

    fn public_encryption_key(&self) -> Option<box_::PublicKey> {
        let app_inner = self.app_inner.borrow();
        Some(app_inner.keys.clone()?.enc_pk)
    }

    fn secret_encryption_key(&self) -> Option<shared_box::SecretKey> {
        let app_inner = self.app_inner.borrow();
        Some(app_inner.keys.clone()?.enc_sk)
    }

    fn secret_symmetric_key(&self) -> Option<shared_secretbox::Key> {
        let app_inner = self.app_inner.borrow();
        Some(app_inner.keys.clone()?.enc_key)
    }

    fn public_bls_key(&self) -> Option<threshold_crypto::PublicKey> {
        let app_inner = self.app_inner.borrow();
        Some(app_inner.keys.clone()?.bls_pk)
    }

    fn secret_bls_key(&self) -> Option<threshold_crypto::SecretKey> {
        let app_inner = self.app_inner.borrow();
        Some(app_inner.keys.clone()?.bls_sk)
    }

    fn owner_key(&self) -> Option<PublicKey> {
        let app_inner = self.app_inner.borrow();
        app_inner.owner_key
    }

    fn compose_message(&self, request: Request) -> Message {
        let message_id = MessageId::new();

        let sig = unwrap!(self.secret_bls_key())
            .sign(&unwrap!(bincode::serialize(&(&request, message_id))));
        Message::Request {
            request,
            message_id,
            signature: Some(safe_nd::Signature::from(sig)),
        }
    }
}

impl Clone for AppClient {
    fn clone(&self) -> Self {
        AppClient {
            inner: Rc::clone(&self.inner),
            app_inner: Rc::clone(&self.app_inner),
        }
    }
}

impl fmt::Debug for AppClient {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Safe App Client")
    }
}

struct AppInner {
    keys: Option<ClientKeys>,
    owner_key: Option<PublicKey>,
    cm_addr: Option<Authority<XorName>>,
    config: Option<BootstrapConfig>,
}

impl AppInner {
    pub fn new(
        keys: Option<ClientKeys>,
        owner_key: Option<PublicKey>,
        cm_addr: Option<Authority<XorName>>,
        config: Option<BootstrapConfig>,
    ) -> AppInner {
        AppInner {
            keys,
            owner_key,
            cm_addr,
            config,
        }
    }
}
