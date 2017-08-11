// Copyright 2017 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement.  This, along with the Licenses can be
// found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

//! App authentication routines

use super::{AuthError, AuthFuture};
use AccessContainerEntry;
use access_container::{access_container, access_container_entry, put_access_container_entry};
use app_container;
use config::{self, AppInfo, Apps};
use futures::{Future, future};
use ipc::update_container_perms;
use routing::ClientError;
use safe_core::{Client, CoreError, FutureExt, MDataInfo, recovery};
use safe_core::ipc::req::AuthReq;
use safe_core::ipc::req::ffi::Permission;
use safe_core::ipc::resp::{AccessContInfo, AppKeys, AuthGranted};
use std::collections::{BTreeSet, HashMap};
use tiny_keccak::sha3_256;

/// Represents current app state
#[derive(Debug, Eq, PartialEq)]
pub enum AppState {
    /// Exists in the authenticator config, access container, and registered in MaidManagers
    Authenticated,
    /// Exists in the authenticator config but not in access container and MaidManagers
    Revoked,
    /// Doesn't exist in the authenticator config
    NotAuthenticated,
}

/// Return a current app state (`Authenticated` if it has an entry
/// in the config file AND the access container, `Revoked` if it has
/// an entry in the config but not in the access container, and `NotAuthenticated`
/// if it's not registered anywhere).
pub fn app_state(client: &Client<()>, apps: &Apps, app_id: String) -> Box<AuthFuture<AppState>> {
    let c2 = client.clone();
    let app_id_hash = sha3_256(app_id.clone().as_bytes());

    if let Some(app) = apps.get(&app_id_hash) {
        let app_keys = app.keys.clone();
        access_container(client)
            .and_then(move |dir| {
                access_container_entry(&c2, &dir, &app_id, app_keys)
            })
            .then(move |res| {
                match res {
                    Ok((_version, Some(_))) => Ok(AppState::Authenticated),
                    Ok((_, None)) |
                        Err(AuthError::CoreError(
                            CoreError::RoutingClientError(
                                ClientError::NoSuchEntry))) => {
                            // App is not in access container, so it is revoked
                            Ok(AppState::Revoked)
                        }
                    Err(e) => Err(e),
                }
            })
            .into_box()
    } else {
        ok!(AppState::NotAuthenticated)
    }
}

/// Store info about the app's dedicated container in the access container
fn insert_app_container(
    mut permissions: AccessContainerEntry,
    app_id: &str,
    app_container_info: MDataInfo,
) -> Box<AuthFuture<AccessContainerEntry>> {
    let access =
        btree_set![
                    Permission::Read,
                    Permission::Insert,
                    Permission::Update,
                    Permission::Delete,
                    Permission::ManagePermissions,
                ];
    let _ = permissions.insert(format!("apps/{}", app_id), (app_container_info, access));
    ok!(permissions)
}

fn update_access_container(
    client: &Client<()>,
    app: &AppInfo,
    permissions: AccessContainerEntry,
) -> Box<AuthFuture<MDataInfo>> {
    let c2 = client.clone();
    let c3 = client.clone();
    let c4 = client.clone();

    let app_info = app.info.clone();
    let app_keys = app.keys.clone();

    access_container(&c2)
        .and_then(move |dir| {
            access_container_entry(&c3, &dir, &app_info.id, app_keys.clone()).then(move |res| {
                let version = match res {
                    // Updating an existing entry
                    Ok((version, _)) => version + 1,
                    // Adding a new access container entry
                    Err(AuthError::CoreError(
                            CoreError::RoutingClientError(
                                ClientError::NoSuchEntry))) => 0,
                    // Error has occurred while trying to get an existing entry
                    Err(e) => return Err(e),
                };
                Ok((version, app_info, app_keys, dir, permissions))
            })
        })
        .and_then(move |(version, app_info, app_keys, dir, permissions)| {
            put_access_container_entry(&c4, &dir, &app_info.id, &app_keys, &permissions, version)
                .map(move |_| dir)
        })
        .into_box()
}

/// Authenticate an app request.
///
/// First, this function searches for an app info in the access container.
/// If the app is found, then the `AppGranted` struct is returned based on that information.
/// If the app is not found in the access container, then it will be authenticated.
pub fn authenticate(client: &Client<()>, auth_req: AuthReq) -> Box<AuthFuture<AuthGranted>> {
    let app_id = auth_req.app.id.clone();
    let permissions = auth_req.containers.clone();
    let app_container = auth_req.app_container;

    let c2 = client.clone();
    let c3 = client.clone();
    let c4 = client.clone();

    config::list_apps(client)
        .join(check_revocation(client, app_id.clone()))
        .and_then(move |((_, apps), ())| {
            app_state(&c2, &apps, app_id.clone())
                .map(move |app_state| {
                    (apps, app_state, app_id)
                })
        })
        .and_then(move |(mut config, app_state, app_id)| {
            // Determine an app state. If it's revoked we can reuse existing
            // keys stored in the config. And if it is authorised, we just
            // return the app info from the config.
            match app_state {
                AppState::NotAuthenticated => {
                    let owner_key = fry!(c3.owner_key().map_err(AuthError::from));
                    let keys = AppKeys::random(owner_key);
                    let app = AppInfo {
                        info: auth_req.app,
                        keys: keys,
                    };
                    config::insert_app(&c3, app.clone())
                        .map(move |_| (app, app_state, app_id))
                        .into_box()
                }
                AppState::Authenticated | AppState::Revoked => {
                    let app_entry_name = sha3_256(app_id.as_bytes());
                    if let Some(app) = config.remove(&app_entry_name) {
                        ok!((app, app_state, app_id))
                    } else {
                        err!(AuthError::Unexpected(
                            "Logical error - couldn't \
                                                                    find a revoked app in config"
                                .to_owned(),
                        ))
                    }
                }
            }
        })
        .and_then(move |(app, app_state, app_id)| {
            match app_state {
                AppState::Authenticated => {
                    // Return info of the already registered app
                    authenticated_app(&c4, app, app_id, app_container)
                }
                AppState::NotAuthenticated |
                AppState::Revoked => {
                    // Register a new app or restore a previously registered app
                    authenticate_new_app(&c4, app, app_container, permissions)
                }
            }
        })
        .into_box()
}

/// Return info of an already registered app.
/// If `app_container` is `true` then we also create/update the dedicated container.
fn authenticated_app(
    client: &Client<()>,
    app: AppInfo,
    app_id: String,
    app_container: bool,
) -> Box<AuthFuture<AuthGranted>> {
    let c2 = client.clone();
    let c3 = client.clone();
    let c4 = client.clone();
    let c5 = client.clone();

    let app_keys = app.keys.clone();
    let app_keys_auth = app.keys.clone();
    let sign_pk = app.keys.sign_pk;
    let bootstrap_config = fry!(Client::<()>::bootstrap_config());

    // Check whether we need to create/update dedicated container
    if app_container {
        access_container(&c2)
            .and_then(move |dir| {
                access_container_entry(&c3, &dir, &app_id, app_keys.clone())
                    .map(move |(_, perms)| (perms, app_id))
            })
            .and_then(move |(perms, app_id)| {
                let perms = perms.unwrap_or_else(AccessContainerEntry::default);
                app_container::fetch(c4, app_id.clone(), sign_pk).map(
                    move |mdata_info| (mdata_info, perms, app_id),
                )
            })
            .and_then(move |(mdata_info, perms, app_id)| {
                insert_app_container(perms, &app_id, mdata_info).and_then(
                    move |perms| update_access_container(&c5, &app, perms),
                )
            })
            .into_box()
    } else {
        access_container(&c4)
    }.and_then(move |dir| {
        let access_container = AccessContInfo::from_mdata_info(dir)?;
        Ok(AuthGranted {
            app_keys: app_keys_auth,
            bootstrap_config: bootstrap_config,
            access_container: access_container,
        })
    })
        .into_box()
}

/// Register a new or revoked app in Maid Managers and in the access container.
///
/// 1. Insert app's key to Maid Managers
/// 2. Update container permissions for requested containers
/// 3. Create the app container (if it's been requested)
/// 4. Insert or update the access container entry for an app
/// 5. Return `AuthGranted`
fn authenticate_new_app(
    client: &Client<()>,
    app: AppInfo,
    app_container: bool,
    permissions: HashMap<String, BTreeSet<Permission>>,
) -> Box<AuthFuture<AuthGranted>> {
    let c2 = client.clone();
    let c3 = client.clone();
    let c4 = client.clone();
    let c5 = client.clone();

    let sign_pk = app.keys.sign_pk;
    let app_keys = app.keys.clone();
    let app_keys_auth = app.keys.clone();
    let app_id = app.info.id.clone();

    client
        .list_auth_keys_and_version()
        .and_then(move |(_, version)| {
            recovery::ins_auth_key(&c2, app_keys.sign_pk, version + 1)
        })
        .map_err(AuthError::from)
        .and_then(move |_| if permissions.is_empty() {
            ok!((Default::default(), sign_pk))
        } else {
            update_container_perms(&c3, permissions, sign_pk)
                .map(move |perms| (perms, sign_pk))
                .into_box()
        })
        .and_then(move |(perms, sign_pk)| {
            if app_container {
                app_container::fetch(c4, app_id.clone(), sign_pk)
                    .and_then(move |mdata_info| {
                        insert_app_container(perms, &app_id, mdata_info)
                    })
                    .into_box()
            } else {
                ok!(perms)
            }.map(move |perms| (perms, app))
        })
        .and_then(move |(perms, app)| {
            update_access_container(&c5, &app, perms)
        })
        .and_then(move |dir| {
            Ok(AuthGranted {
                app_keys: app_keys_auth,
                bootstrap_config: Client::<()>::bootstrap_config()?,
                access_container: AccessContInfo::from_mdata_info(dir)?,
            })
        })
        .into_box()
}

fn check_revocation(client: &Client<()>, app_id: String) -> Box<AuthFuture<()>> {
    config::get_revocation_queue(client)
        .map(|queue| if let Some((_, queue)) = queue {
            queue
        } else {
            Default::default()
        })
        .and_then(move |queue| if queue.contains(&app_id) {
            future::err(AuthError::from(
                "Couldn't authenticate app that is pending revocation",
            ))
        } else {
            future::ok(())
        })
        .into_box()
}
