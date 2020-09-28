// Copyright (c) 2020 rust-mysql-simple contributors
//
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. All files in the project carrying such notice may not be copied,
// modified, or distributed except according to those terms.

use mysql_common::{
    crypto,
    io::ReadMysqlExt,
    named_params::parse_named_params,
    packets::{
        column_from_payload, parse_auth_switch_request, parse_err_packet, parse_handshake_packet,
        parse_ok_packet, AuthPlugin, AuthSwitchRequest, Column, ComStmtClose,
        ComStmtExecuteRequestBuilder, ComStmtSendLongData, HandshakePacket, HandshakeResponse,
        OkPacket, OkPacketKind, SslRequest,
    },
    proto::{codec::Compression, sync_framed::MySyncFramed},
    value::{read_bin_values, read_text_values, ServerSide},
};

use std::{
    borrow::{Borrow, Cow},
    cmp,
    collections::HashMap,
    convert::TryFrom,
    io::{self, Read, Write as _},
    mem,
    ops::{Deref, DerefMut},
    process,
    sync::Arc,
};

use crate::{
    conn::{
        local_infile::LocalInfile,
        pool::{Pool, PooledConn},
        query_result::{Binary, Or, Text},
        stmt::{InnerStmt, Statement},
        stmt_cache::StmtCache,
        transaction::{AccessMode, TxOpts},
    },
    consts::{CapabilityFlags, Command, StatusFlags, MAX_PAYLOAD_LEN},
    from_value, from_value_opt,
    io::Stream,
    prelude::*,
    DriverError::{
        MismatchedStmtParams, NamedParamsForPositionalQuery, Protocol41NotSet,
        ReadOnlyTransNotSupported, SetupError, TlsNotSupported, UnexpectedPacket,
        UnknownAuthPlugin, UnsupportedProtocol,
    },
    Error::{self, DriverError, MySqlError},
    LocalInfileHandler, Opts, OptsBuilder, Params, QueryResult, Result, SslOpts, Transaction,
    Value::{self, Bytes, NULL},
};

pub mod local_infile;
pub mod opts;
pub mod pool;
pub mod query;
pub mod query_result;
pub mod queryable;
pub mod stmt;
mod stmt_cache;
pub mod transaction;

/// Mutable connection.
#[derive(Debug)]
pub enum ConnMut<'c, 't, 'tc> {
    Mut(&'c mut Conn),
    TxMut(&'t mut Transaction<'tc>),
    Owned(Conn),
    Pooled(PooledConn),
}

impl From<Conn> for ConnMut<'static, 'static, 'static> {
    fn from(conn: Conn) -> Self {
        ConnMut::Owned(conn)
    }
}

impl From<PooledConn> for ConnMut<'static, 'static, 'static> {
    fn from(conn: PooledConn) -> Self {
        ConnMut::Pooled(conn)
    }
}

impl<'a> From<&'a mut Conn> for ConnMut<'a, 'static, 'static> {
    fn from(conn: &'a mut Conn) -> Self {
        ConnMut::Mut(conn)
    }
}

impl<'a> From<&'a mut PooledConn> for ConnMut<'a, 'static, 'static> {
    fn from(conn: &'a mut PooledConn) -> Self {
        ConnMut::Mut(conn.as_mut())
    }
}

impl<'t, 'tc> From<&'t mut Transaction<'tc>> for ConnMut<'static, 't, 'tc> {
    fn from(tx: &'t mut Transaction<'tc>) -> Self {
        ConnMut::TxMut(tx)
    }
}

impl TryFrom<&Pool> for ConnMut<'static, 'static, 'static> {
    type Error = Error;

    fn try_from(pool: &Pool) -> Result<Self> {
        pool.get_conn().map(From::from)
    }
}

impl Deref for ConnMut<'_, '_, '_> {
    type Target = Conn;

    fn deref(&self) -> &Conn {
        match self {
            ConnMut::Mut(conn) => &**conn,
            ConnMut::TxMut(tx) => &*tx.conn,
            ConnMut::Owned(conn) => &conn,
            ConnMut::Pooled(conn) => conn.as_ref(),
        }
    }
}

impl DerefMut for ConnMut<'_, '_, '_> {
    fn deref_mut(&mut self) -> &mut Conn {
        match self {
            ConnMut::Mut(ref mut conn) => &mut **conn,
            ConnMut::TxMut(tx) => &mut *tx.conn,
            ConnMut::Owned(ref mut conn) => conn,
            ConnMut::Pooled(ref mut conn) => conn.as_mut(),
        }
    }
}

/// Connection internals.
#[derive(Debug)]
struct ConnInner {
    opts: Opts,
    stream: Option<MySyncFramed<Stream>>,
    stmt_cache: StmtCache,
    server_version: Option<(u16, u16, u16)>,
    mariadb_server_version: Option<(u16, u16, u16)>,
    /// Last Ok packet, if any.
    ok_packet: Option<OkPacket<'static>>,
    capability_flags: CapabilityFlags,
    connection_id: u32,
    status_flags: StatusFlags,
    character_set: u8,
    last_command: u8,
    connected: bool,
    has_results: bool,
    local_infile_handler: Option<LocalInfileHandler>,
}

impl ConnInner {
    fn empty<T: Into<Opts>>(opts: T) -> Self {
        let opts = opts.into();
        ConnInner {
            stmt_cache: StmtCache::new(opts.get_stmt_cache_size()),
            opts,
            stream: None,
            capability_flags: CapabilityFlags::empty(),
            status_flags: StatusFlags::empty(),
            connection_id: 0u32,
            character_set: 0u8,
            ok_packet: None,
            last_command: 0u8,
            connected: false,
            has_results: false,
            server_version: None,
            mariadb_server_version: None,
            local_infile_handler: None,
        }
    }
}

/// Mysql connection.
#[derive(Debug)]
pub struct Conn(Box<ConnInner>);

impl Conn {
    /// Returns connection identifier.
    pub fn connection_id(&self) -> u32 {
        self.0.connection_id
    }

    /// Returns number of rows affected by the last query.
    pub fn affected_rows(&self) -> u64 {
        self.0
            .ok_packet
            .as_ref()
            .map(OkPacket::affected_rows)
            .unwrap_or_default()
    }

    /// Returns last insert id of the last query.
    ///
    /// Returns zero if there was no last insert id.
    pub fn last_insert_id(&self) -> u64 {
        self.0
            .ok_packet
            .as_ref()
            .and_then(OkPacket::last_insert_id)
            .unwrap_or_default()
    }

    /// Returns number of warnings, reported by the server.
    pub fn warnings(&self) -> u16 {
        self.0
            .ok_packet
            .as_ref()
            .map(OkPacket::warnings)
            .unwrap_or_default()
    }

    /// [Info], reported by the server.
    ///
    /// Will be empty if not defined.
    ///
    /// [Info]: http://dev.mysql.com/doc/internals/en/packet-OK_Packet.html
    pub fn info_ref(&self) -> &[u8] {
        self.0
            .ok_packet
            .as_ref()
            .and_then(OkPacket::info_ref)
            .unwrap_or_default()
    }

    /// [Info], reported by the server.
    ///
    /// Will be empty if not defined.
    ///
    /// [Info]: http://dev.mysql.com/doc/internals/en/packet-OK_Packet.html
    pub fn info_str(&self) -> Cow<str> {
        self.0
            .ok_packet
            .as_ref()
            .and_then(OkPacket::info_str)
            .unwrap_or_default()
    }

    fn stream_ref(&self) -> &MySyncFramed<Stream> {
        self.0.stream.as_ref().expect("incomplete connection")
    }

    fn stream_mut(&mut self) -> &mut MySyncFramed<Stream> {
        self.0.stream.as_mut().expect("incomplete connection")
    }

    fn is_insecure(&self) -> bool {
        self.stream_ref().get_ref().is_insecure()
    }

    fn is_socket(&self) -> bool {
        self.stream_ref().get_ref().is_socket()
    }

    /// Check the connection can be improved.
    #[allow(unused_assignments)]
    fn can_improved(&mut self) -> Result<Option<Opts>> {
        if self.0.opts.get_prefer_socket() && self.0.opts.addr_is_loopback() {
            let mut socket = None;
            #[cfg(test)]
            {
                socket = self.0.opts.0.injected_socket.clone();
            }
            if socket.is_none() {
                socket = self.get_system_var("socket")?.map(from_value::<String>);
            }
            if let Some(socket) = socket {
                if self.0.opts.get_socket().is_none() {
                    let socket_opts = OptsBuilder::from_opts(self.0.opts.clone());
                    if !socket.is_empty() {
                        return Ok(Some(socket_opts.socket(Some(socket)).into()));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Creates new `Conn`.
    pub fn new<T: Into<Opts>>(opts: T) -> Result<Conn> {
        let mut conn = Conn(Box::new(ConnInner::empty(opts)));
        conn.connect_stream()?;
        conn.connect()?;
        let mut conn = {
            if let Some(new_opts) = conn.can_improved()? {
                let mut improved_conn = Conn(Box::new(ConnInner::empty(new_opts)));
                improved_conn
                    .connect_stream()
                    .and_then(|_| {
                        improved_conn.connect()?;
                        Ok(improved_conn)
                    })
                    .unwrap_or(conn)
            } else {
                conn
            }
        };
        for cmd in conn.0.opts.get_init() {
            conn.query_drop(cmd)?;
        }
        Ok(conn)
    }

    fn soft_reset(&mut self) -> Result<()> {
        self.write_command(Command::COM_RESET_CONNECTION, &[])?;
        self.read_packet().and_then(|pld| match pld[0] {
            0 => {
                let ok = parse_ok_packet(&*pld, self.0.capability_flags, OkPacketKind::Other)?;
                self.handle_ok(&ok);
                self.0.last_command = 0;
                self.0.stmt_cache.clear();
                Ok(())
            }
            _ => {
                let err = parse_err_packet(&*pld, self.0.capability_flags)?;
                Err(MySqlError(err.into()))
            }
        })
    }

    fn hard_reset(&mut self) -> Result<()> {
        self.0.stream = None;
        self.0.stmt_cache.clear();
        self.0.capability_flags = CapabilityFlags::empty();
        self.0.status_flags = StatusFlags::empty();
        self.0.connection_id = 0;
        self.0.character_set = 0;
        self.0.ok_packet = None;
        self.0.last_command = 0;
        self.0.connected = false;
        self.0.has_results = false;
        self.connect_stream()?;
        self.connect()
    }

    /// Resets `MyConn` (drops state then reconnects).
    pub fn reset(&mut self) -> Result<()> {
        match (self.0.server_version, self.0.mariadb_server_version) {
            (Some(ref version), _) if *version > (5, 7, 3) => {
                self.soft_reset().or_else(|_| self.hard_reset())
            }
            (_, Some(ref version)) if *version >= (10, 2, 7) => {
                self.soft_reset().or_else(|_| self.hard_reset())
            }
            _ => self.hard_reset(),
        }
    }

    fn switch_to_ssl(&mut self, ssl_opts: SslOpts) -> Result<()> {
        let stream = self.0.stream.take().expect("incomplete conn");
        let (in_buf, out_buf, codec, stream) = stream.destruct();
        let stream = stream.make_secure(self.0.opts.get_host(), ssl_opts)?;
        let stream = MySyncFramed::construct(in_buf, out_buf, codec, stream);
        self.0.stream = Some(stream);
        Ok(())
    }

    fn connect_stream(&mut self) -> Result<()> {
        let opts = &self.0.opts;
        let read_timeout = opts.get_read_timeout().cloned();
        let write_timeout = opts.get_write_timeout().cloned();
        let tcp_keepalive_time = opts.get_tcp_keepalive_time_ms();
        let tcp_nodelay = opts.get_tcp_nodelay();
        let tcp_connect_timeout = opts.get_tcp_connect_timeout();
        let bind_address = opts.bind_address().cloned();
        let stream = if let Some(socket) = opts.get_socket() {
            Stream::connect_socket(&*socket, read_timeout, write_timeout)?
        } else {
            let port = opts.get_tcp_port();
            let ip_or_hostname = match opts.get_host() {
                url::Host::Domain(domain) => domain,
                url::Host::Ipv4(ip) => ip.to_string(),
                url::Host::Ipv6(ip) => ip.to_string(),
            };
            Stream::connect_tcp(
                &*ip_or_hostname,
                port,
                read_timeout,
                write_timeout,
                tcp_keepalive_time,
                tcp_nodelay,
                tcp_connect_timeout,
                bind_address,
            )?
        };
        self.0.stream = Some(MySyncFramed::new(stream));
        Ok(())
    }

    fn read_packet(&mut self) -> Result<Vec<u8>> {
        let data = self.stream_mut().next().transpose()?.ok_or(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "server disconnected",
        ))?;
        match data[0] {
            0xff => {
                let error_packet = parse_err_packet(&*data, self.0.capability_flags)?;
                self.handle_err();
                Err(MySqlError(error_packet.into()))
            }
            _ => Ok(data),
        }
    }

    fn drop_packet(&mut self) -> Result<()> {
        self.read_packet().map(|_| ())
    }

    fn write_packet<T: Into<Vec<u8>>>(&mut self, data: T) -> Result<()> {
        self.stream_mut().send(data.into())?;
        Ok(())
    }

    fn handle_handshake(&mut self, hp: &HandshakePacket<'_>) {
        self.0.capability_flags = hp.capabilities() & self.get_client_flags();
        self.0.status_flags = hp.status_flags();
        self.0.connection_id = hp.connection_id();
        self.0.character_set = hp.default_collation();
        self.0.server_version = hp.server_version_parsed();
        self.0.mariadb_server_version = hp.maria_db_server_version_parsed();
    }

    fn handle_ok(&mut self, op: &OkPacket<'_>) {
        self.0.status_flags = op.status_flags();
        self.0.ok_packet = Some(op.clone().into_owned());
    }

    fn handle_err(&mut self) {
        self.0.has_results = false;
        self.0.ok_packet = None;
    }

    fn more_results_exists(&self) -> bool {
        self.0
            .status_flags
            .contains(StatusFlags::SERVER_MORE_RESULTS_EXISTS)
    }

    fn perform_auth_switch(&mut self, auth_switch_request: AuthSwitchRequest<'_>) -> Result<()> {
        let nonce = auth_switch_request.plugin_data();
        let plugin_data = auth_switch_request
            .auth_plugin()
            .gen_data(self.0.opts.get_pass(), nonce);
        self.write_packet(plugin_data.unwrap_or_else(Vec::new))?;
        self.continue_auth(auth_switch_request.auth_plugin(), nonce, true)
    }

    fn do_handshake(&mut self) -> Result<()> {
        let payload = self.read_packet()?;
        let handshake = parse_handshake_packet(payload.as_ref())?;

        if handshake.protocol_version() != 10u8 {
            return Err(DriverError(UnsupportedProtocol(
                handshake.protocol_version(),
            )));
        }

        if !handshake
            .capabilities()
            .contains(CapabilityFlags::CLIENT_PROTOCOL_41)
        {
            return Err(DriverError(Protocol41NotSet));
        }

        self.handle_handshake(&handshake);

        if self.is_insecure() {
            if let Some(ssl_opts) = self.0.opts.get_ssl_opts().cloned() {
                if !handshake
                    .capabilities()
                    .contains(CapabilityFlags::CLIENT_SSL)
                {
                    return Err(DriverError(TlsNotSupported));
                } else {
                    self.do_ssl_request()?;
                    self.switch_to_ssl(ssl_opts)?;
                }
            }
        }

        let nonce = handshake.nonce();

        let auth_plugin = handshake
            .auth_plugin()
            .unwrap_or(&AuthPlugin::MysqlNativePassword);
        if let AuthPlugin::Other(ref name) = auth_plugin {
            let plugin_name = String::from_utf8_lossy(name).into();
            Err(DriverError(UnknownAuthPlugin(plugin_name)))?
        }

        let auth_data = auth_plugin.gen_data(self.0.opts.get_pass(), &*nonce);
        self.write_handshake_response(auth_plugin, auth_data.as_ref().map(AsRef::as_ref))?;

        self.continue_auth(auth_plugin, &*nonce, false)?;

        if self
            .0
            .capability_flags
            .contains(CapabilityFlags::CLIENT_COMPRESS)
        {
            self.switch_to_compressed();
        }

        Ok(())
    }

    fn switch_to_compressed(&mut self) {
        self.stream_mut()
            .codec_mut()
            .compress(Compression::default());
    }

    fn get_client_flags(&self) -> CapabilityFlags {
        let mut client_flags = CapabilityFlags::CLIENT_PROTOCOL_41
            | CapabilityFlags::CLIENT_SECURE_CONNECTION
            | CapabilityFlags::CLIENT_LONG_PASSWORD
            | CapabilityFlags::CLIENT_TRANSACTIONS
            | CapabilityFlags::CLIENT_LOCAL_FILES
            | CapabilityFlags::CLIENT_MULTI_STATEMENTS
            | CapabilityFlags::CLIENT_MULTI_RESULTS
            | CapabilityFlags::CLIENT_PS_MULTI_RESULTS
            | CapabilityFlags::CLIENT_PLUGIN_AUTH
            | CapabilityFlags::CLIENT_CONNECT_ATTRS
            | (self.0.capability_flags & CapabilityFlags::CLIENT_LONG_FLAG);
        if self.0.opts.get_compress().is_some() {
            client_flags.insert(CapabilityFlags::CLIENT_COMPRESS);
        }
        if let Some(db_name) = self.0.opts.get_db_name() {
            if !db_name.is_empty() {
                client_flags.insert(CapabilityFlags::CLIENT_CONNECT_WITH_DB);
            }
        }
        if self.is_insecure() && self.0.opts.get_ssl_opts().is_some() {
            client_flags.insert(CapabilityFlags::CLIENT_SSL);
        }
        client_flags | self.0.opts.get_additional_capabilities()
    }

    fn connect_attrs(&self) -> HashMap<String, String> {
        let program_name = match self.0.opts.get_connect_attrs().get("program_name") {
            Some(program_name) => program_name.clone(),
            None => {
                let arg0 = std::env::args_os().next();
                let arg0 = arg0.as_ref().map(|x| x.to_string_lossy());
                arg0.unwrap_or("".into()).to_owned().to_string()
            }
        };

        let mut attrs = HashMap::new();

        attrs.insert("_client_name".into(), "rust-mysql-simple".into());
        attrs.insert("_client_version".into(), env!("CARGO_PKG_VERSION").into());
        attrs.insert("_os".into(), env!("CARGO_CFG_TARGET_OS").into());
        attrs.insert("_pid".into(), process::id().to_string());
        attrs.insert("_platform".into(), env!("CARGO_CFG_TARGET_ARCH").into());
        attrs.insert("program_name".into(), program_name.to_string());

        for (name, value) in self.0.opts.get_connect_attrs().clone() {
            attrs.insert(name, value);
        }

        attrs
    }

    fn do_ssl_request(&mut self) -> Result<()> {
        let ssl_request = SslRequest::new(self.get_client_flags());
        self.write_packet(ssl_request)
    }

    fn write_handshake_response(
        &mut self,
        auth_plugin: &AuthPlugin<'_>,
        scramble_buf: Option<&[u8]>,
    ) -> Result<()> {
        let handshake_response = HandshakeResponse::new(
            &scramble_buf,
            self.0.server_version.unwrap_or((0, 0, 0)),
            self.0.opts.get_user(),
            self.0.opts.get_db_name(),
            auth_plugin,
            self.0.capability_flags,
            &self.connect_attrs(),
        );
        self.write_packet(handshake_response)
    }

    fn continue_auth(
        &mut self,
        auth_plugin: &AuthPlugin<'_>,
        nonce: &[u8],
        auth_switched: bool,
    ) -> Result<()> {
        match auth_plugin {
            AuthPlugin::MysqlNativePassword => {
                self.continue_mysql_native_password_auth(auth_switched)
            }
            AuthPlugin::CachingSha2Password => {
                self.continue_caching_sha2_password_auth(nonce, auth_switched)
            }
            AuthPlugin::Other(ref name) => {
                let plugin_name = String::from_utf8_lossy(name).into();
                Err(DriverError(UnknownAuthPlugin(plugin_name)))?
            }
        }
    }

    fn continue_mysql_native_password_auth(&mut self, auth_switched: bool) -> Result<()> {
        let payload = self.read_packet()?;

        match payload[0] {
            // auth ok
            0x00 => {
                let ok = parse_ok_packet(&*payload, self.0.capability_flags, OkPacketKind::Other)?;
                self.handle_ok(&ok);
                Ok(())
            }
            // auth switch
            0xfe if !auth_switched => {
                let auth_switch_request = parse_auth_switch_request(&*payload)?;
                self.perform_auth_switch(auth_switch_request)
            }
            _ => Err(DriverError(UnexpectedPacket)),
        }
    }

    fn continue_caching_sha2_password_auth(
        &mut self,
        nonce: &[u8],
        auth_switched: bool,
    ) -> Result<()> {
        let payload = self.read_packet()?;

        match payload[0] {
            0x00 => {
                // ok packet for empty password
                Ok(())
            }
            0x01 => match payload[1] {
                0x03 => {
                    let payload = self.read_packet()?;
                    let ok =
                        parse_ok_packet(&*payload, self.0.capability_flags, OkPacketKind::Other)?;
                    self.handle_ok(&ok);
                    Ok(())
                }
                0x04 => {
                    if !self.is_insecure() || self.is_socket() {
                        let mut pass = self
                            .0
                            .opts
                            .get_pass()
                            .map(Vec::from)
                            .unwrap_or_else(Vec::new);
                        pass.push(0);
                        self.write_packet(pass)?;
                    } else {
                        self.write_packet(vec![0x02])?;
                        let payload = self.read_packet()?;
                        let key = &payload[1..];
                        let mut pass = self
                            .0
                            .opts
                            .get_pass()
                            .map(Vec::from)
                            .unwrap_or_else(Vec::new);
                        pass.push(0);
                        for i in 0..pass.len() {
                            pass[i] ^= nonce[i % nonce.len()];
                        }
                        let encrypted_pass = crypto::encrypt(&*pass, key);
                        self.write_packet(encrypted_pass)?;
                    }

                    let payload = self.read_packet()?;
                    let ok =
                        parse_ok_packet(&*payload, self.0.capability_flags, OkPacketKind::Other)?;
                    self.handle_ok(&ok);
                    Ok(())
                }
                _ => Err(DriverError(UnexpectedPacket)),
            },
            0xfe if !auth_switched => {
                let auth_switch_request = parse_auth_switch_request(&*payload)?;
                self.perform_auth_switch(auth_switch_request)
            }
            _ => Err(DriverError(UnexpectedPacket)),
        }
    }

    fn reset_seq_id(&mut self) {
        self.stream_mut().codec_mut().reset_seq_id();
    }

    fn sync_seq_id(&mut self) {
        self.stream_mut().codec_mut().sync_seq_id();
    }

    fn write_command_raw<T: Into<Vec<u8>>>(&mut self, body: T) -> Result<()> {
        let body = body.into();
        self.reset_seq_id();
        self.0.last_command = body[0];
        self.write_packet(body)
    }

    fn write_command(&mut self, cmd: Command, data: &[u8]) -> Result<()> {
        let mut body = Vec::with_capacity(1 + data.len());
        body.push(cmd as u8);
        body.extend_from_slice(data);

        self.write_command_raw(body)
    }

    fn send_long_data(&mut self, stmt_id: u32, params: &[Value]) -> Result<()> {
        for (i, value) in params.into_iter().enumerate() {
            match value {
                Bytes(bytes) => {
                    let chunks = bytes.chunks(MAX_PAYLOAD_LEN - 6);
                    let chunks = chunks.chain(if bytes.is_empty() {
                        Some(&[][..])
                    } else {
                        None
                    });
                    for chunk in chunks {
                        let com = ComStmtSendLongData::new(stmt_id, i, chunk);
                        self.write_command_raw(com)?;
                    }
                }
                _ => (),
            }
        }

        Ok(())
    }

    fn _execute(
        &mut self,
        stmt: &Statement,
        params: Params,
    ) -> Result<Or<Vec<Column>, OkPacket<'static>>> {
        let exec_request = match params {
            Params::Empty => {
                if stmt.num_params() != 0 {
                    return Err(DriverError(MismatchedStmtParams(stmt.num_params(), 0)));
                }

                let (body, _) = ComStmtExecuteRequestBuilder::new(stmt.id()).build(&[]);
                body
            }
            Params::Positional(params) => {
                if stmt.num_params() != params.len() as u16 {
                    return Err(DriverError(MismatchedStmtParams(
                        stmt.num_params(),
                        params.len(),
                    )));
                }

                let (body, as_long_data) =
                    ComStmtExecuteRequestBuilder::new(stmt.id()).build(&*params);

                if as_long_data {
                    self.send_long_data(stmt.id(), &*params)?;
                }

                body
            }
            Params::Named(_) => {
                if stmt.named_params.is_none() {
                    return Err(DriverError(NamedParamsForPositionalQuery));
                }
                let named_params = stmt.named_params.as_ref().unwrap();
                return self._execute(stmt, params.into_positional(named_params)?);
            }
        };
        self.write_command_raw(exec_request)?;
        self.handle_result_set()
    }

    fn _start_transaction(&mut self, tx_opts: TxOpts) -> Result<()> {
        if let Some(i_level) = tx_opts.isolation_level() {
            self.query_drop(format!("SET TRANSACTION ISOLATION LEVEL {}", i_level))?;
        }
        if let Some(mode) = tx_opts.access_mode() {
            let supported = match (self.0.server_version, self.0.mariadb_server_version) {
                (Some(ref version), _) if *version >= (5, 6, 5) => true,
                (_, Some(ref version)) if *version >= (10, 0, 0) => true,
                _ => false,
            };
            if !supported {
                return Err(DriverError(ReadOnlyTransNotSupported));
            }
            match mode {
                AccessMode::ReadOnly => self.query_drop("SET TRANSACTION READ ONLY")?,
                AccessMode::ReadWrite => self.query_drop("SET TRANSACTION READ WRITE")?,
            }
        }
        if tx_opts.with_consistent_snapshot() {
            self.query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT")?;
        } else {
            self.query_drop("START TRANSACTION")?;
        };
        Ok(())
    }

    fn send_local_infile(&mut self, file_name: &[u8]) -> Result<OkPacket<'static>> {
        {
            let buffer_size = cmp::min(
                MAX_PAYLOAD_LEN - 4,
                self.stream_ref().codec().max_allowed_packet - 4,
            );
            let chunk = vec![0u8; buffer_size].into_boxed_slice();
            let maybe_handler = self
                .0
                .local_infile_handler
                .clone()
                .or_else(|| self.0.opts.get_local_infile_handler().cloned());
            let mut local_infile = LocalInfile::new(io::Cursor::new(chunk), self);
            if let Some(handler) = maybe_handler {
                // Unwrap won't panic because we have exclusive access to `self` and this
                // method is not re-entrant, because `LocalInfile` does not expose the
                // connection.
                let handler_fn = &mut *handler.0.lock().unwrap();
                handler_fn(file_name, &mut local_infile)?;
            }
            local_infile.flush()?;
        }
        self.write_packet(Vec::new())?;
        let pld = self.read_packet()?;
        let ok = parse_ok_packet(pld.as_ref(), self.0.capability_flags, OkPacketKind::Other)?;
        self.handle_ok(&ok);
        Ok(ok.into_owned())
    }

    fn handle_result_set(&mut self) -> Result<Or<Vec<Column>, OkPacket<'static>>> {
        if self.more_results_exists() {
            self.sync_seq_id();
        }

        let pld = self.read_packet()?;
        match pld[0] {
            0x00 => {
                let ok =
                    parse_ok_packet(pld.as_ref(), self.0.capability_flags, OkPacketKind::Other)?;
                self.handle_ok(&ok);
                Ok(Or::B(ok.into_owned()))
            }
            0xfb => {
                let mut reader = &pld[1..];
                let mut file_name = Vec::with_capacity(reader.len());
                reader.read_to_end(&mut file_name)?;
                match self.send_local_infile(file_name.as_ref()) {
                    Ok(ok) => Ok(Or::B(ok)),
                    Err(err) => Err(err),
                }
            }
            _ => {
                let mut reader = &pld[..];
                let column_count = reader.read_lenenc_int()?;
                let mut columns: Vec<Column> = Vec::with_capacity(column_count as usize);
                for _ in 0..column_count {
                    let pld = self.read_packet()?;
                    columns.push(column_from_payload(pld)?);
                }
                // skip eof packet
                self.read_packet()?;
                self.0.has_results = column_count > 0;
                Ok(Or::A(columns))
            }
        }
    }

    fn _query(&mut self, query: &str) -> Result<Or<Vec<Column>, OkPacket<'static>>> {
        self.write_command(Command::COM_QUERY, query.as_bytes())?;
        self.handle_result_set()
    }

    /// Executes [`COM_PING`](http://dev.mysql.com/doc/internals/en/com-ping.html)
    /// on `Conn`. Return `true` on success or `false` on error.
    pub fn ping(&mut self) -> bool {
        match self.write_command(Command::COM_PING, &[]) {
            Ok(_) => self.drop_packet().is_ok(),
            _ => false,
        }
    }

    /// Executes [`COM_INIT_DB`](https://dev.mysql.com/doc/internals/en/com-init-db.html)
    /// on `Conn`.
    pub fn select_db(&mut self, schema: &str) -> bool {
        match self.write_command(Command::COM_INIT_DB, schema.as_bytes()) {
            Ok(_) => self.drop_packet().is_ok(),
            _ => false,
        }
    }

    /// Starts new transaction with provided options.
    /// `readonly` is only available since MySQL 5.6.5.
    pub fn start_transaction(&mut self, tx_opts: TxOpts) -> Result<Transaction> {
        self._start_transaction(tx_opts)?;
        Ok(Transaction::new(self.into()))
    }

    fn _true_prepare(&mut self, query: &str) -> Result<InnerStmt> {
        self.write_command(Command::COM_STMT_PREPARE, query.as_bytes())?;
        let pld = self.read_packet()?;
        let mut stmt = InnerStmt::from_payload(pld.as_ref(), self.connection_id())?;
        if stmt.num_params() > 0 {
            let mut params: Vec<Column> = Vec::with_capacity(stmt.num_params() as usize);
            for _ in 0..stmt.num_params() {
                let pld = self.read_packet()?;
                params.push(column_from_payload(pld)?);
            }
            stmt = stmt.with_params(Some(params));
            self.read_packet()?;
        }
        if stmt.num_columns() > 0 {
            let mut columns: Vec<Column> = Vec::with_capacity(stmt.num_columns() as usize);
            for _ in 0..stmt.num_columns() {
                let pld = self.read_packet()?;
                columns.push(column_from_payload(pld)?);
            }
            stmt = stmt.with_columns(Some(columns));
            self.read_packet()?;
        }
        Ok(stmt)
    }

    fn _prepare(&mut self, query: &str) -> Result<Arc<InnerStmt>> {
        if let Some(entry) = self.0.stmt_cache.by_query(query) {
            return Ok(entry.stmt.clone());
        }

        let inner_st = Arc::new(self._true_prepare(query)?);

        if let Some(old_stmt) = self
            .0
            .stmt_cache
            .put(Arc::new(query.into()), inner_st.clone())
        {
            self.close(Statement::new(old_stmt, None))?;
        }

        Ok(inner_st)
    }

    fn connect(&mut self) -> Result<()> {
        if self.0.connected {
            return Ok(());
        }
        self.do_handshake()
            .and_then(|_| {
                Ok(from_value_opt::<usize>(
                    self.get_system_var("max_allowed_packet")?.unwrap_or(NULL),
                )
                .unwrap_or(0))
            })
            .and_then(|max_allowed_packet| {
                if max_allowed_packet == 0 {
                    Err(DriverError(SetupError))
                } else {
                    self.stream_mut().codec_mut().max_allowed_packet = max_allowed_packet;
                    self.0.connected = true;
                    Ok(())
                }
            })
    }

    fn get_system_var(&mut self, name: &str) -> Result<Option<Value>> {
        self.query_first(format!("SELECT @@{}", name))
    }

    fn next_bin(&mut self, columns: &[Column]) -> Result<Option<Vec<Value>>> {
        if !self.0.has_results {
            return Ok(None);
        }
        let pld = self.read_packet()?;
        if pld[0] == 0xfe && pld.len() < 0xfe {
            self.0.has_results = false;
            let p = parse_ok_packet(
                pld.as_ref(),
                self.0.capability_flags,
                OkPacketKind::ResultSetTerminator,
            )?;
            self.handle_ok(&p);
            return Ok(None);
        }
        let values = read_bin_values::<ServerSide>(&*pld, columns)?;
        Ok(Some(values))
    }

    fn next_text(&mut self, col_count: usize) -> Result<Option<Vec<Value>>> {
        if !self.0.has_results {
            return Ok(None);
        }
        let pld = self.read_packet()?;
        if pld[0] == 0xfe && pld.len() < 0xfe {
            self.0.has_results = false;
            let p = parse_ok_packet(
                pld.as_ref(),
                self.0.capability_flags,
                OkPacketKind::ResultSetTerminator,
            )?;
            self.handle_ok(&p);
            return Ok(None);
        }
        let values = read_text_values(&*pld, col_count)?;
        Ok(Some(values))
    }

    fn has_stmt(&self, query: &str) -> bool {
        self.0.stmt_cache.contains_query(query)
    }

    /// Sets a callback to handle requests for local files. These are
    /// caused by using `LOAD DATA LOCAL INFILE` queries. The
    /// callback is passed the filename, and a `Write`able object
    /// to receive the contents of that file.
    /// Specifying `None` will reset the handler to the one specified
    /// in the `Opts` for this connection.
    pub fn set_local_infile_handler(&mut self, handler: Option<LocalInfileHandler>) {
        self.0.local_infile_handler = handler;
    }

    pub fn no_backslash_escape(&self) -> bool {
        self.0
            .status_flags
            .contains(StatusFlags::SERVER_STATUS_NO_BACKSLASH_ESCAPES)
    }
}

impl Queryable for Conn {
    fn query_iter<T: AsRef<str>>(&mut self, query: T) -> Result<QueryResult<'_, '_, '_, Text>> {
        let meta = self._query(query.as_ref())?;
        Ok(QueryResult::new(ConnMut::Mut(self), meta))
    }

    fn prep<T: AsRef<str>>(&mut self, query: T) -> Result<Statement> {
        let query = query.as_ref();
        let (named_params, real_query) = parse_named_params(query)?;
        self._prepare(real_query.borrow())
            .map(|inner| Statement::new(inner, named_params))
    }

    fn close(&mut self, stmt: Statement) -> Result<()> {
        self.0.stmt_cache.remove(stmt.id());
        let com_stmt_close = ComStmtClose::new(stmt.id());
        self.write_command_raw(com_stmt_close)?;
        Ok(())
    }

    fn exec_iter<S, P>(&mut self, stmt: S, params: P) -> Result<QueryResult<'_, '_, '_, Binary>>
    where
        S: AsStatement,
        P: Into<Params>,
    {
        let statement = stmt.as_statement(self)?;
        let meta = self._execute(&*statement, params.into())?;
        Ok(QueryResult::new(ConnMut::Mut(self), meta))
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        let stmt_cache = mem::replace(&mut self.0.stmt_cache, StmtCache::new(0));

        for (_, entry) in stmt_cache.into_iter() {
            let _ = self.close(Statement::new(entry.stmt, None));
        }

        if self.0.stream.is_some() {
            let _ = self.write_command(Command::COM_QUIT, &[]);
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod test {
    mod my_conn {
        use std::{collections::HashMap, io::Write, iter, process};

        use crate::{
            from_row, from_value, params,
            prelude::*,
            test_misc::get_opts,
            time::PrimitiveDateTime,
            Conn,
            DriverError::{MissingNamedParameter, NamedParamsForPositionalQuery},
            Error::DriverError,
            LocalInfileHandler, Opts, OptsBuilder, Params, Pool, TxOpts,
            Value::{self, Bytes, Date, Float, Int, NULL},
        };

        fn get_system_variable<T>(conn: &mut Conn, name: &str) -> T
        where
            T: FromValue,
        {
            conn.query_first::<(String, T), _>(format!("show variables like '{}'", name))
                .unwrap()
                .unwrap()
                .1
        }

        #[test]
        fn should_connect() {
            let mut conn = Conn::new(get_opts()).unwrap();

            let mode: String = conn
                .query_first("SELECT @@GLOBAL.sql_mode")
                .unwrap()
                .unwrap();
            assert!(mode.contains("TRADITIONAL"));
            assert!(conn.ping());

            if crate::test_misc::test_compression() {
                assert!(format!("{:?}", conn.0.stream).contains("Compression"));
            }

            if crate::test_misc::test_ssl() {
                assert!(!conn.is_insecure());
            }
        }

        #[test]
        fn mysql_async_issue_107() -> crate::Result<()> {
            let mut conn = Conn::new(get_opts())?;
            conn.query_drop(
                r"CREATE TEMPORARY TABLE mysql.issue (
                        a BIGINT(20) UNSIGNED,
                        b VARBINARY(16),
                        c BINARY(32),
                        d BIGINT(20) UNSIGNED,
                        e BINARY(32)
                    )",
            )?;
            conn.query_drop(
                r"INSERT INTO mysql.issue VALUES (
                        0,
                        0xC066F966B0860000,
                        0x7939DA98E524C5F969FC2DE8D905FD9501EBC6F20001B0A9C941E0BE6D50CF44,
                        0,
                        ''
                    ), (
                        1,
                        '',
                        0x076311DF4D407B0854371BA13A5F3FB1A4555AC22B361375FD47B263F31822F2,
                        0,
                        ''
                    )",
            )?;

            let q = "SELECT b, c, d, e FROM mysql.issue";
            let result = conn.query_iter(q)?;

            let loaded_structs = result
                .map(|row| crate::from_row::<(Vec<u8>, Vec<u8>, u64, Vec<u8>)>(row.unwrap()))
                .collect::<Vec<_>>();

            assert_eq!(loaded_structs.len(), 2);

            Ok(())
        }

        #[test]
        fn query_traits() -> Result<(), Box<dyn std::error::Error>> {
            macro_rules! test_query {
                ($conn : expr) => {
                    "CREATE TEMPORARY TABLE tmp (a INT)".run($conn)?;

                    "INSERT INTO tmp (a) VALUES (?)".with((42,)).run($conn)?;

                    "INSERT INTO tmp (a) VALUES (?)"
                        .with((43..=44).map(|x| (x,)))
                        .batch($conn)?;

                    let first: Option<u8> = "SELECT a FROM tmp LIMIT 1".first($conn)?;
                    assert_eq!(first, Some(42), "first text");

                    let first: Option<u8> = "SELECT a FROM tmp LIMIT 1".with(()).first($conn)?;
                    assert_eq!(first, Some(42), "first bin");

                    let count = "SELECT a FROM tmp".run($conn)?.count();
                    assert_eq!(count, 3, "run text");

                    let count = "SELECT a FROM tmp".with(()).run($conn)?.count();
                    assert_eq!(count, 3, "run bin");

                    let all: Vec<u8> = "SELECT a FROM tmp".fetch($conn)?;
                    assert_eq!(all, vec![42, 43, 44], "fetch text");

                    let all: Vec<u8> = "SELECT a FROM tmp".with(()).fetch($conn)?;
                    assert_eq!(all, vec![42, 43, 44], "fetch bin");

                    let mapped = "SELECT a FROM tmp".map($conn, |x: u8| x + 1)?;
                    assert_eq!(mapped, vec![43, 44, 45], "map text");

                    let mapped = "SELECT a FROM tmp".with(()).map($conn, |x: u8| x + 1)?;
                    assert_eq!(mapped, vec![43, 44, 45], "map bin");

                    let sum = "SELECT a FROM tmp".fold($conn, 0_u8, |acc, x: u8| acc + x)?;
                    assert_eq!(sum, 42 + 43 + 44, "fold text");

                    let sum = "SELECT a FROM tmp"
                        .with(())
                        .fold($conn, 0_u8, |acc, x: u8| acc + x)?;
                    assert_eq!(sum, 42 + 43 + 44, "fold bin");

                    "DROP TABLE tmp".run($conn)?;
                };
            }

            let mut conn = Conn::new(get_opts())?;

            let mut tx = conn.start_transaction(TxOpts::default())?;
            test_query!(&mut tx);
            tx.rollback()?;

            test_query!(&mut conn);

            let pool = Pool::new(get_opts())?;
            let mut pooled_conn = pool.get_conn()?;

            let mut tx = pool.start_transaction(TxOpts::default())?;
            test_query!(&mut tx);
            tx.rollback()?;

            test_query!(&mut pooled_conn);

            Ok(())
        }

        #[test]
        #[should_panic(expected = "Could not connect to address")]
        fn should_fail_on_wrong_socket_path() {
            let opts = OptsBuilder::from_opts(get_opts()).socket(Some("/foo/bar/baz"));
            let _ = Conn::new(opts).unwrap();
        }

        #[test]
        fn should_fallback_to_tcp_if_cant_switch_to_socket() {
            let mut opts = Opts::from(get_opts());
            opts.0.injected_socket = Some("/foo/bar/baz".into());
            let _ = Conn::new(opts).unwrap();
        }

        #[test]
        fn should_connect_with_database() {
            const DB_NAME: &str = "mysql";

            let opts = OptsBuilder::from_opts(get_opts()).db_name(Some(DB_NAME));

            let mut conn = Conn::new(opts).unwrap();

            let db_name: String = conn.query_first("SELECT DATABASE()").unwrap().unwrap();
            assert_eq!(db_name, DB_NAME);
        }

        #[test]
        fn should_connect_by_hostname() {
            let opts = OptsBuilder::from_opts(get_opts()).ip_or_hostname(Some("localhost"));
            let mut conn = Conn::new(opts).unwrap();
            assert!(conn.ping());
        }

        #[test]
        fn should_select_db() {
            const DB_NAME: &str = "t_select_db";

            let mut conn = Conn::new(get_opts()).unwrap();
            conn.query_drop(format!("CREATE DATABASE IF NOT EXISTS {}", DB_NAME))
                .unwrap();
            assert!(conn.select_db(DB_NAME));

            let db_name: String = conn.query_first("SELECT DATABASE()").unwrap().unwrap();
            assert_eq!(db_name, DB_NAME);

            conn.query_drop(format!("DROP DATABASE {}", DB_NAME))
                .unwrap();
        }

        #[test]
        fn should_execute_queryes_and_parse_results() {
            type TestRow = (String, String, String, String, String, String);

            const CREATE_QUERY: &str = r"CREATE TEMPORARY TABLE mysql.tbl
                (id SERIAL, a TEXT, b INT, c INT UNSIGNED, d DATE, e FLOAT)";
            const INSERT_QUERY_1: &str = r"INSERT
                INTO mysql.tbl(a, b, c, d, e)
                VALUES ('hello', -123, 123, '2014-05-05', 123.123)";
            const INSERT_QUERY_2: &str = r"INSERT
                INTO mysql.tbl(a, b, c, d, e)
                VALUES ('world', -321, 321, '2014-06-06', 321.321)";

            let mut conn = Conn::new(get_opts()).unwrap();

            conn.query_drop(CREATE_QUERY).unwrap();
            assert_eq!(conn.affected_rows(), 0);
            assert_eq!(conn.last_insert_id(), 0);

            conn.query_drop(INSERT_QUERY_1).unwrap();
            assert_eq!(conn.affected_rows(), 1);
            assert_eq!(conn.last_insert_id(), 1);

            conn.query_drop(INSERT_QUERY_2).unwrap();
            assert_eq!(conn.affected_rows(), 1);
            assert_eq!(conn.last_insert_id(), 2);

            conn.query_drop("SELECT * FROM unexisted").unwrap_err();
            conn.query_iter("SELECT * FROM mysql.tbl").unwrap(); // Drop::drop for QueryResult

            conn.query_drop("UPDATE mysql.tbl SET a = 'foo'").unwrap();
            assert_eq!(conn.affected_rows(), 2);
            assert_eq!(conn.last_insert_id(), 0);

            assert!(conn
                .query_first::<TestRow, _>("SELECT * FROM mysql.tbl WHERE a = 'bar'")
                .unwrap()
                .is_none());

            let rows: Vec<TestRow> = conn.query("SELECT * FROM mysql.tbl").unwrap();
            assert_eq!(
                rows,
                vec![
                    (
                        "1".into(),
                        "foo".into(),
                        "-123".into(),
                        "123".into(),
                        "2014-05-05".into(),
                        "123.123".into()
                    ),
                    (
                        "2".into(),
                        "foo".into(),
                        "-321".into(),
                        "321".into(),
                        "2014-06-06".into(),
                        "321.321".into()
                    )
                ]
            );
        }

        #[test]
        fn should_parse_large_text_result() {
            let mut conn = Conn::new(get_opts()).unwrap();
            let value: Value = conn
                .query_first("SELECT REPEAT('A', 20000000)")
                .unwrap()
                .unwrap();
            assert_eq!(value, Bytes(iter::repeat(b'A').take(20_000_000).collect()));
        }

        #[test]
        fn should_execute_statements_and_parse_results() {
            const CREATE_QUERY: &str = r"CREATE TEMPORARY TABLE
                mysql.tbl (a TEXT, b INT, c INT UNSIGNED, d DATE, e FLOAT)";
            const INSERT_SMTM: &str = r"INSERT
                INTO mysql.tbl (a, b, c, d, e)
                VALUES (?, ?, ?, ?, ?)";

            type RowType = (Value, Value, Value, Value, Value);

            let row1 = (
                Bytes(b"hello".to_vec()),
                Int(-123_i64),
                Int(123_i64),
                Date(2014_u16, 5_u8, 5_u8, 0_u8, 0_u8, 0_u8, 0_u32),
                Float(123.123_f32),
            );
            let row2 = (Bytes(b"".to_vec()), NULL, NULL, NULL, Float(321.321_f32));

            let mut conn = Conn::new(get_opts()).unwrap();
            conn.query_drop(CREATE_QUERY).unwrap();

            let insert_stmt = conn.prep(INSERT_SMTM).unwrap();
            assert_eq!(insert_stmt.connection_id(), conn.connection_id());
            conn.exec_drop(
                &insert_stmt,
                (
                    from_value::<String>(row1.0.clone()),
                    from_value::<i32>(row1.1.clone()),
                    from_value::<u32>(row1.2.clone()),
                    from_value::<PrimitiveDateTime>(row1.3.clone()),
                    from_value::<f32>(row1.4.clone()),
                ),
            )
            .unwrap();
            conn.exec_drop(
                &insert_stmt,
                (
                    from_value::<String>(row2.0.clone()),
                    row2.1.clone(),
                    row2.2.clone(),
                    row2.3.clone(),
                    from_value::<f32>(row2.4.clone()),
                ),
            )
            .unwrap();

            let select_stmt = conn.prep("SELECT * from mysql.tbl").unwrap();
            let rows: Vec<RowType> = conn.exec(&select_stmt, ()).unwrap();

            assert_eq!(rows, vec![row1, row2]);
        }

        #[test]
        fn should_parse_large_binary_result() {
            let mut conn = Conn::new(get_opts()).unwrap();
            let stmt = conn.prep("SELECT REPEAT('A', 20000000)").unwrap();
            let value: Value = conn.exec_first(&stmt, ()).unwrap().unwrap();
            assert_eq!(value, Bytes(iter::repeat(b'A').take(20_000_000).collect()));
        }

        #[test]
        fn manually_closed_stmt() {
            let opts = OptsBuilder::from(get_opts()).stmt_cache_size(1);
            let mut conn = Conn::new(opts).unwrap();
            let stmt = conn.prep("SELECT 1").unwrap();
            conn.exec_drop(&stmt, ()).unwrap();
            conn.close(stmt).unwrap();
            let stmt = conn.prep("SELECT 1").unwrap();
            conn.exec_drop(&stmt, ()).unwrap();
            conn.close(stmt).unwrap();
            let stmt = conn.prep("SELECT 2").unwrap();
            conn.exec_drop(&stmt, ()).unwrap();
        }

        #[test]
        fn should_start_commit_and_rollback_transactions() {
            let mut conn = Conn::new(get_opts()).unwrap();
            conn.query_drop(
                "CREATE TEMPORARY TABLE mysql.tbl(id INT NOT NULL PRIMARY KEY AUTO_INCREMENT, a INT)",
            )
            .unwrap();
            let _ = conn
                .start_transaction(TxOpts::default())
                .and_then(|mut t| {
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(1)").unwrap();
                    assert_eq!(t.last_insert_id(), Some(1));
                    assert_eq!(t.affected_rows(), 1);
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(2)").unwrap();
                    t.commit().unwrap();
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                conn.query_iter("SELECT COUNT(a) from mysql.tbl")
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                vec![Bytes(b"2".to_vec())]
            );
            let _ = conn
                .start_transaction(TxOpts::default())
                .and_then(|mut t| {
                    t.query_drop("INSERT INTO tbl2(a) VALUES(1)").unwrap_err();
                    Ok(())
                    // implicit rollback
                })
                .unwrap();
            assert_eq!(
                conn.query_iter("SELECT COUNT(a) from mysql.tbl")
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                vec![Bytes(b"2".to_vec())]
            );
            let _ = conn
                .start_transaction(TxOpts::default())
                .and_then(|mut t| {
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(1)").unwrap();
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(2)").unwrap();
                    t.rollback().unwrap();
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                conn.query_iter("SELECT COUNT(a) from mysql.tbl")
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                vec![Bytes(b"2".to_vec())]
            );
            let mut tx = conn.start_transaction(TxOpts::default()).unwrap();
            tx.exec_drop("INSERT INTO mysql.tbl(a) VALUES(?)", (3,))
                .unwrap();
            tx.exec_drop("INSERT INTO mysql.tbl(a) VALUES(?)", (4,))
                .unwrap();
            tx.commit().unwrap();
            assert_eq!(
                conn.query_iter("SELECT COUNT(a) from mysql.tbl")
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                vec![Bytes(b"4".to_vec())]
            );
            let mut tx = conn.start_transaction(TxOpts::default()).unwrap();
            tx.exec_drop("INSERT INTO mysql.tbl(a) VALUES(?)", (5,))
                .unwrap();
            tx.exec_drop("INSERT INTO mysql.tbl(a) VALUES(?)", (6,))
                .unwrap();
            drop(tx);
            assert_eq!(
                conn.query_first("SELECT COUNT(a) from mysql.tbl").unwrap(),
                Some(4_usize),
            );
        }
        #[test]
        fn should_handle_LOCAL_INFILE_with_custom_handler() {
            let mut conn = Conn::new(get_opts()).unwrap();
            conn.query_drop("CREATE TEMPORARY TABLE mysql.tbl(a TEXT)")
                .unwrap();
            conn.set_local_infile_handler(Some(LocalInfileHandler::new(|_, stream| {
                let mut cell_data = vec![b'Z'; 65535];
                cell_data.push(b'\n');
                for _ in 0..1536 {
                    stream.write_all(&*cell_data)?;
                }
                Ok(())
            })));
            match conn.query_drop("LOAD DATA LOCAL INFILE 'file_name' INTO TABLE mysql.tbl") {
                Ok(_) => {}
                Err(ref err) if format!("{}", err).find("not allowed").is_some() => {
                    return;
                }
                Err(err) => panic!("ERROR {}", err),
            }
            let count = conn
                .query_iter("SELECT * FROM mysql.tbl")
                .unwrap()
                .map(|row| {
                    assert_eq!(from_row::<(Vec<u8>,)>(row.unwrap()).0.len(), 65535);
                    1
                })
                .sum::<usize>();
            assert_eq!(count, 1536);
        }

        #[test]
        fn should_reset_connection() {
            let mut conn = Conn::new(get_opts()).unwrap();
            conn.query_drop(
                "CREATE TEMPORARY TABLE `mysql`.`test` \
                 (`test` VARCHAR(255) NULL);",
            )
            .unwrap();
            conn.query_drop("INSERT INTO `mysql`.`test` (`test`) VALUES ('foo');")
                .unwrap();
            assert_eq!(conn.affected_rows(), 1);
            conn.reset().unwrap();
            assert_eq!(conn.affected_rows(), 0);
            conn.query_drop("SELECT * FROM `mysql`.`test`;")
                .unwrap_err();
        }

        #[test]
        fn prep_exec() {
            let mut conn = Conn::new(get_opts()).unwrap();

            let stmt1 = conn.prep("SELECT :foo").unwrap();
            let stmt2 = conn.prep("SELECT :bar").unwrap();
            assert_eq!(
                conn.exec::<String, _, _>(&stmt1, params! { "foo" => "foo" })
                    .unwrap(),
                vec![String::from("foo")],
            );
            assert_eq!(
                conn.exec::<String, _, _>(&stmt2, params! { "bar" => "bar" })
                    .unwrap(),
                vec![String::from("bar")],
            );
        }

        #[test]
        fn should_connect_via_socket_for_127_0_0_1() {
            let opts = OptsBuilder::from_opts(get_opts());
            let conn = Conn::new(opts).unwrap();
            if conn.is_insecure() {
                assert!(conn.is_socket());
            }
        }

        #[test]
        fn should_connect_via_socket_localhost() {
            let opts = OptsBuilder::from_opts(get_opts()).ip_or_hostname(Some("localhost"));
            let conn = Conn::new(opts).unwrap();
            if conn.is_insecure() {
                assert!(conn.is_socket());
            }
        }

        #[test]
        fn should_drop_multi_result_set() {
            let opts = OptsBuilder::from_opts(get_opts()).db_name(Some("mysql"));
            let mut conn = Conn::new(opts).unwrap();
            conn.query_drop("CREATE TEMPORARY TABLE TEST_TABLE ( name varchar(255) )")
                .unwrap();
            conn.exec_drop("SELECT * FROM TEST_TABLE", ()).unwrap();
            conn.query_drop(
                r"
                INSERT INTO TEST_TABLE (name) VALUES ('one');
                INSERT INTO TEST_TABLE (name) VALUES ('two');
                INSERT INTO TEST_TABLE (name) VALUES ('three');",
            )
            .unwrap();
            conn.exec_drop("SELECT * FROM TEST_TABLE", ()).unwrap();
        }

        #[test]
        fn should_handle_multi_resultset() {
            let opts = OptsBuilder::from_opts(get_opts())
                .prefer_socket(false)
                .db_name(Some("mysql"));
            let mut conn = Conn::new(opts).unwrap();
            conn.query_drop("DROP PROCEDURE IF EXISTS multi").unwrap();
            conn.query_drop(
                r#"CREATE PROCEDURE multi() BEGIN
                        SELECT 1 UNION ALL SELECT 2;
                        DO 1;
                        SELECT 3 UNION ALL SELECT 4;
                        DO 1;
                        DO 1;
                        SELECT REPEAT('A', 17000000);
                        SELECT REPEAT('A', 17000000);
                    END"#,
            )
            .unwrap();
            {
                let mut query_result = conn.query_iter("CALL multi()").unwrap();
                let result_set = query_result
                    .by_ref()
                    .map(|row| row.unwrap().unwrap().pop().unwrap())
                    .collect::<Vec<crate::Value>>();
                assert_eq!(result_set, vec![Bytes(b"1".to_vec()), Bytes(b"2".to_vec())]);
                let result_set = query_result
                    .by_ref()
                    .map(|row| row.unwrap().unwrap().pop().unwrap())
                    .collect::<Vec<crate::Value>>();
                assert_eq!(result_set, vec![Bytes(b"3".to_vec()), Bytes(b"4".to_vec())]);
            }
            let mut result = conn.query_iter("SELECT 1; SELECT 2; SELECT 3;").unwrap();
            let mut i = 0;
            while let Some(result_set) = result.next_set() {
                i += 1;
                for row in result_set.unwrap() {
                    match i {
                        1 => assert_eq!(row.unwrap().unwrap(), vec![Bytes(b"1".to_vec())]),
                        2 => assert_eq!(row.unwrap().unwrap(), vec![Bytes(b"2".to_vec())]),
                        3 => assert_eq!(row.unwrap().unwrap(), vec![Bytes(b"3".to_vec())]),
                        _ => unreachable!(),
                    }
                }
            }
            assert_eq!(i, 3);
        }

        #[test]
        fn should_work_with_named_params() {
            let mut conn = Conn::new(get_opts()).unwrap();
            {
                let stmt = conn.prep("SELECT :a, :b, :a, :c").unwrap();
                let result = conn
                    .exec_first(&stmt, params! {"a" => 1, "b" => 2, "c" => 3})
                    .unwrap()
                    .unwrap();
                assert_eq!((1_u8, 2_u8, 1_u8, 3_u8), result);
            }

            let result = conn
                .exec_first(
                    "SELECT :a, :b, :a + :b, :c",
                    params! {
                        "a" => 1,
                        "b" => 2,
                        "c" => 3,
                    },
                )
                .unwrap()
                .unwrap();
            assert_eq!((1_u8, 2_u8, 3_u8, 3_u8), result);
        }

        #[test]
        fn should_return_error_on_missing_named_parameter() {
            let mut conn = Conn::new(get_opts()).unwrap();
            let stmt = conn.prep("SELECT :a, :b, :a, :c, :d").unwrap();
            let result =
                conn.exec_first::<crate::Row, _, _>(&stmt, params! {"a" => 1, "b" => 2, "c" => 3,});
            match result {
                Err(DriverError(MissingNamedParameter(ref x))) if x == "d" => (),
                _ => assert!(false),
            }
        }

        #[test]
        fn should_return_error_on_named_params_for_positional_statement() {
            let mut conn = Conn::new(get_opts()).unwrap();
            let stmt = conn.prep("SELECT ?, ?, ?, ?, ?").unwrap();
            let result = conn.exec_drop(&stmt, params! {"a" => 1, "b" => 2, "c" => 3,});
            match result {
                Err(DriverError(NamedParamsForPositionalQuery)) => (),
                _ => assert!(false),
            }
        }

        #[test]
        fn should_handle_tcp_connect_timeout() {
            use crate::error::{DriverError::ConnectTimeout, Error::DriverError};

            let opts = OptsBuilder::from_opts(get_opts())
                .prefer_socket(false)
                .tcp_connect_timeout(Some(::std::time::Duration::from_millis(1000)));
            assert!(Conn::new(opts).unwrap().ping());

            let opts = OptsBuilder::from_opts(get_opts())
                .prefer_socket(false)
                .tcp_connect_timeout(Some(::std::time::Duration::from_millis(1000)))
                .ip_or_hostname(Some("192.168.255.255"));
            match Conn::new(opts).unwrap_err() {
                DriverError(ConnectTimeout) => {}
                err => panic!("Unexpected error: {}", err),
            }
        }

        #[test]
        fn should_set_additional_capabilities() {
            use crate::consts::CapabilityFlags;

            let opts = OptsBuilder::from_opts(get_opts())
                .additional_capabilities(CapabilityFlags::CLIENT_FOUND_ROWS);

            let mut conn = Conn::new(opts).unwrap();
            conn.query_drop("CREATE TEMPORARY TABLE mysql.tbl (a INT, b TEXT)")
                .unwrap();
            conn.query_drop("INSERT INTO mysql.tbl (a, b) VALUES (1, 'foo')")
                .unwrap();
            let result = conn
                .query_iter("UPDATE mysql.tbl SET b = 'foo' WHERE a = 1")
                .unwrap();
            assert_eq!(result.affected_rows(), 1);
        }

        #[test]
        fn should_bind_before_connect() {
            let port = 28000 + (rand::random::<u16>() % 2000);
            let opts = OptsBuilder::from_opts(get_opts())
                .prefer_socket(false)
                .ip_or_hostname(Some("127.0.0.1"))
                .bind_address(Some(([127, 0, 0, 1], port)));
            let conn = Conn::new(opts).unwrap();
            let debug_format: String = format!("{:?}", conn);
            let expected_1 = format!("addr: V4(127.0.0.1:{})", port);
            let expected_2 = format!("addr: 127.0.0.1:{}", port);
            assert!(
                debug_format.contains(&expected_1) || debug_format.contains(&expected_2),
                "debug_format: {}",
                debug_format
            );
        }

        #[test]
        fn should_bind_before_connect_with_timeout() {
            let port = 30000 + (rand::random::<u16>() % 2000);
            let opts = OptsBuilder::from_opts(get_opts())
                .prefer_socket(false)
                .ip_or_hostname(Some("127.0.0.1"))
                .bind_address(Some(([127, 0, 0, 1], port)))
                .tcp_connect_timeout(Some(::std::time::Duration::from_millis(1000)));
            let mut conn = Conn::new(opts).unwrap();
            assert!(conn.ping());
            let debug_format: String = format!("{:?}", conn);
            let expected_1 = format!("addr: V4(127.0.0.1:{})", port);
            let expected_2 = format!("addr: 127.0.0.1:{}", port);
            assert!(
                debug_format.contains(&expected_1) || debug_format.contains(&expected_2),
                "debug_format: {}",
                debug_format
            );
        }

        #[test]
        fn should_not_cache_statements_if_stmt_cache_size_is_zero() {
            let opts = OptsBuilder::from_opts(get_opts()).stmt_cache_size(0);
            let mut conn = Conn::new(opts).unwrap();

            let stmt1 = conn.prep("DO 1").unwrap();
            let stmt2 = conn.prep("DO 2").unwrap();
            let stmt3 = conn.prep("DO 3").unwrap();

            conn.close(stmt1).unwrap();
            conn.close(stmt2).unwrap();
            conn.close(stmt3).unwrap();

            let status: (Value, u8) = conn
                .query_first("SHOW SESSION STATUS LIKE 'Com_stmt_close';")
                .unwrap()
                .unwrap();
            assert_eq!(status.1, 3);
        }

        #[test]
        fn should_hold_stmt_cache_size_bounds() {
            let opts = OptsBuilder::from_opts(get_opts()).stmt_cache_size(3);
            let mut conn = Conn::new(opts).unwrap();

            conn.prep("DO 1").unwrap();
            conn.prep("DO 2").unwrap();
            conn.prep("DO 3").unwrap();
            conn.prep("DO 1").unwrap();
            conn.prep("DO 4").unwrap();
            conn.prep("DO 3").unwrap();
            conn.prep("DO 5").unwrap();
            conn.prep("DO 6").unwrap();

            let status: (String, usize) = conn
                .query_first("SHOW SESSION STATUS LIKE 'Com_stmt_close'")
                .unwrap()
                .unwrap();

            assert_eq!(status.1, 3);

            let mut order = conn
                .0
                .stmt_cache
                .iter()
                .map(|(_, entry)| &**entry.query.0.as_ref())
                .collect::<Vec<&str>>();
            order.sort();
            assert_eq!(order, &["DO 3", "DO 5", "DO 6"]);
        }

        #[test]
        fn should_handle_json_columns() {
            use crate::{Deserialized, Serialized};
            use serde_json::Value as Json;
            use std::str::FromStr;

            #[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
            pub struct DecTest {
                foo: String,
                quux: (u64, String),
            }

            let decodable = DecTest {
                foo: "bar".into(),
                quux: (42, "hello".into()),
            };

            let mut conn = Conn::new(get_opts()).unwrap();
            if conn
                .query_drop("CREATE TEMPORARY TABLE mysql.tbl(a VARCHAR(32), b JSON)")
                .is_err()
            {
                conn.query_drop("CREATE TEMPORARY TABLE mysql.tbl(a VARCHAR(32), b TEXT)")
                    .unwrap();
            }
            conn.exec_drop(
                r#"INSERT INTO mysql.tbl VALUES ('hello', ?)"#,
                (Serialized(&decodable),),
            )
            .unwrap();

            let (a, b): (String, Json) = conn
                .query_first("SELECT a, b FROM mysql.tbl")
                .unwrap()
                .unwrap();
            assert_eq!(
                (a, b),
                (
                    "hello".into(),
                    Json::from_str(r#"{"foo": "bar", "quux": [42, "hello"]}"#).unwrap()
                )
            );

            let row = conn
                .exec_first("SELECT a, b FROM mysql.tbl WHERE a = ?", ("hello",))
                .unwrap()
                .unwrap();
            let (a, Deserialized(b)) = from_row(row);
            assert_eq!((a, b), (String::from("hello"), decodable));
        }

        #[test]
        fn should_set_connect_attrs() {
            let opts = OptsBuilder::from_opts(get_opts());
            let mut conn = Conn::new(opts).unwrap();

            let support_connect_attrs = match (conn.0.server_version, conn.0.mariadb_server_version)
            {
                (Some(ref version), _) if *version >= (5, 6, 0) => true,
                (_, Some(ref version)) if *version >= (10, 0, 0) => true,
                _ => false,
            };

            if support_connect_attrs {
                // MySQL >= 5.6 or MariaDB >= 10.0

                if get_system_variable::<String>(&mut conn, "performance_schema") != "ON" {
                    panic!("The system variable `performance_schema` is off. Restart the MySQL server with `--performance_schema=on` to pass the test.");
                }
                let attrs_size: i32 =
                    get_system_variable(&mut conn, "performance_schema_session_connect_attrs_size");
                if attrs_size >= 0 && attrs_size <= 128 {
                    panic!("The system variable `performance_schema_session_connect_attrs_size` is {}. Restart the MySQL server with `--performance_schema_session_connect_attrs_size=-1` to pass the test.", attrs_size);
                }

                fn assert_connect_attrs(conn: &mut Conn, expected_values: &[(&str, &str)]) {
                    let mut actual_values = HashMap::new();
                    for row in conn.query_iter("SELECT attr_name, attr_value FROM performance_schema.session_account_connect_attrs WHERE processlist_id = connection_id()").unwrap() {
                        let (name, value) = from_row::<(String, String)>(row.unwrap());
                        actual_values.insert(name, value);
                    }

                    for (name, value) in expected_values {
                        assert_eq!(
                            actual_values.get(&name.to_string()),
                            Some(&value.to_string())
                        );
                    }
                }

                let pid = process::id().to_string();
                let progname = std::env::args_os()
                    .next()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let mut expected_values = vec![
                    ("_client_name", "rust-mysql-simple"),
                    ("_client_version", env!("CARGO_PKG_VERSION")),
                    ("_os", env!("CARGO_CFG_TARGET_OS")),
                    ("_pid", &pid),
                    ("_platform", env!("CARGO_CFG_TARGET_ARCH")),
                    ("program_name", &progname),
                ];

                // No connect attributes are added.
                assert_connect_attrs(&mut conn, &expected_values);

                // Connect attributes are added.
                let opts = OptsBuilder::from_opts(get_opts());
                let mut connect_attrs = HashMap::with_capacity(3);
                connect_attrs.insert("foo", "foo val");
                connect_attrs.insert("bar", "bar val");
                connect_attrs.insert("program_name", "my program name");
                let mut conn = Conn::new(opts.connect_attrs(connect_attrs)).unwrap();
                expected_values.pop(); // remove program_name at the last
                expected_values.push(("foo", "foo val"));
                expected_values.push(("bar", "bar val"));
                expected_values.push(("program_name", "my program name"));
                assert_connect_attrs(&mut conn, &expected_values);
            }
        }
    }

    #[cfg(feature = "nightly")]
    mod bench {
        use test;

        use crate::{params, prelude::*, test_misc::get_opts, Conn, Value::NULL};

        #[bench]
        fn simple_exec(bencher: &mut test::Bencher) {
            let mut conn = Conn::new(get_opts()).unwrap();
            bencher.iter(|| {
                let _ = conn.query_drop("DO 1");
            })
        }

        #[bench]
        fn prepared_exec(bencher: &mut test::Bencher) {
            let mut conn = Conn::new(get_opts()).unwrap();
            let stmt = conn.prep("DO 1").unwrap();
            bencher.iter(|| {
                let _ = conn.exec_drop(&stmt, ()).unwrap();
            })
        }

        #[bench]
        fn prepare_and_exec(bencher: &mut test::Bencher) {
            let mut conn = Conn::new(get_opts()).unwrap();
            bencher.iter(|| {
                let stmt = conn.prep("SELECT ?").unwrap();
                let _ = conn.exec_drop(&stmt, (0,)).unwrap();
            })
        }

        #[bench]
        fn simple_query_row(bencher: &mut test::Bencher) {
            let mut conn = Conn::new(get_opts()).unwrap();
            bencher.iter(|| {
                let _ = conn.query_drop("SELECT 1").unwrap();
            })
        }

        #[bench]
        fn simple_prepared_query_row(bencher: &mut test::Bencher) {
            let mut conn = Conn::new(get_opts()).unwrap();
            let stmt = conn.prep("SELECT 1").unwrap();
            bencher.iter(|| {
                let _ = conn.exec_drop(&stmt, ()).unwrap();
            })
        }

        #[bench]
        fn simple_prepared_query_row_with_param(bencher: &mut test::Bencher) {
            let mut conn = Conn::new(get_opts()).unwrap();
            let stmt = conn.prep("SELECT ?").unwrap();
            bencher.iter(|| {
                let _ = conn.exec_drop(&stmt, (0,)).unwrap();
            })
        }

        #[bench]
        fn simple_prepared_query_row_with_named_param(bencher: &mut test::Bencher) {
            let mut conn = Conn::new(get_opts()).unwrap();
            let stmt = conn.prep("SELECT :a").unwrap();
            bencher.iter(|| {
                let _ = conn.exec_drop(&stmt, params! {"a" => 0}).unwrap();
            })
        }

        #[bench]
        fn simple_prepared_query_row_with_5_params(bencher: &mut test::Bencher) {
            let mut conn = Conn::new(get_opts()).unwrap();
            let stmt = conn.prep("SELECT ?, ?, ?, ?, ?").unwrap();
            let params = (42i8, b"123456".to_vec(), 1.618f64, NULL, 1i8);
            bencher.iter(|| {
                let _ = conn.exec_drop(&stmt, &params).unwrap();
            })
        }

        #[bench]
        fn simple_prepared_query_row_with_5_named_params(bencher: &mut test::Bencher) {
            let mut conn = Conn::new(get_opts()).unwrap();
            let stmt = conn
                .prep("SELECT :one, :two, :three, :four, :five")
                .unwrap();
            bencher.iter(|| {
                let _ = conn.exec_drop(
                    &stmt,
                    params! {
                        "one" => 42i8,
                        "two" => b"123456",
                        "three" => 1.618f64,
                        "four" => NULL,
                        "five" => 1i8,
                    },
                );
            })
        }

        #[bench]
        fn select_large_string(bencher: &mut test::Bencher) {
            let mut conn = Conn::new(get_opts()).unwrap();
            bencher.iter(|| {
                let _ = conn.query_drop("SELECT REPEAT('A', 10000)").unwrap();
            })
        }

        #[bench]
        fn select_prepared_large_string(bencher: &mut test::Bencher) {
            let mut conn = Conn::new(get_opts()).unwrap();
            let stmt = conn.prep("SELECT REPEAT('A', 10000)").unwrap();
            bencher.iter(|| {
                let _ = conn.exec_drop(&stmt, ()).unwrap();
            })
        }

        #[bench]
        fn many_small_rows(bencher: &mut test::Bencher) {
            let mut conn = Conn::new(get_opts()).unwrap();
            conn.query_drop("CREATE TEMPORARY TABLE mysql.x (id INT)")
                .unwrap();
            for _ in 0..512 {
                conn.query_drop("INSERT INTO mysql.x VALUES (256)").unwrap();
            }
            let stmt = conn.prep("SELECT * FROM mysql.x").unwrap();
            bencher.iter(|| {
                let _ = conn.exec_drop(&stmt, ()).unwrap();
            });
        }
    }
}
