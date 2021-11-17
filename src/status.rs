use std::sync::Arc;

use bytes::BytesMut;
use minecraft_protocol::data::chat::{Message, Payload};
use minecraft_protocol::data::server_status::*;
use minecraft_protocol::decoder::Decoder;
use minecraft_protocol::encoder::Encoder;
use minecraft_protocol::version::v1_14_4::handshake::Handshake;
use minecraft_protocol::version::v1_14_4::login::LoginStart;
use minecraft_protocol::version::v1_14_4::status::StatusResponse;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::config::*;
use crate::join;
use crate::proto::action;
use crate::proto::client::{Client, ClientInfo, ClientState};
use crate::proto::packet::{self, RawPacket};
use crate::proto::packets;
use crate::server::{self, Server};

/// Proxy the given inbound stream to a target address.
// TODO: do not drop error here, return Box<dyn Error>
pub async fn serve(
    client: Client,
    mut inbound: TcpStream,
    config: Arc<Config>,
    server: Arc<Server>,
) -> Result<(), ()> {
    let (mut reader, mut writer) = inbound.split();

    // Incoming buffer and packet holding queue
    let mut buf = BytesMut::new();

    // Remember inbound packets, track client info
    let mut inbound_history = BytesMut::new();
    let mut client_info = ClientInfo::empty();

    loop {
        // Read packet from stream
        let (packet, raw) = match packet::read_packet(&client, &mut buf, &mut reader).await {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(_) => {
                error!(target: "lazymc", "Closing connection, error occurred");
                break;
            }
        };

        // Grab client state
        let client_state = client.state();

        // Hijack handshake
        if client_state == ClientState::Handshake
            && packet.id == packets::handshake::SERVER_HANDSHAKE
        {
            // Parse handshake
            let handshake = match Handshake::decode(&mut packet.data.as_slice()) {
                Ok(handshake) => handshake,
                Err(_) => {
                    debug!(target: "lazymc", "Got malformed handshake from client, disconnecting");
                    break;
                }
            };

            // Parse new state
            let new_state = match ClientState::from_id(handshake.next_state) {
                Some(state) => state,
                None => {
                    error!(target: "lazymc", "Client tried to switch into unknown protcol state ({}), disconnecting", handshake.next_state);
                    break;
                }
            };

            // Update client info and client state
            client_info
                .protocol_version
                .replace(handshake.protocol_version);
            client.set_state(new_state);

            // If loggin in with handshake, remember inbound
            if new_state == ClientState::Login {
                inbound_history.extend(raw);
            }

            continue;
        }

        // Hijack server status packet
        if client_state == ClientState::Status && packet.id == packets::status::SERVER_STATUS {
            let server_status = server_status(&config, &server).await;
            let packet = StatusResponse { server_status };

            let mut data = Vec::new();
            packet.encode(&mut data).map_err(|_| ())?;

            let response = RawPacket::new(0, data).encode(&client)?;
            writer.write_all(&response).await.map_err(|_| ())?;

            continue;
        }

        // Hijack ping packet
        if client_state == ClientState::Status && packet.id == packets::status::SERVER_PING {
            writer.write_all(&raw).await.map_err(|_| ())?;
            continue;
        }

        // Hijack login start
        if client_state == ClientState::Login && packet.id == packets::login::SERVER_LOGIN_START {
            // Try to get login username, update client info
            // TODO: we should always parse this packet successfully
            let username = LoginStart::decode(&mut packet.data.as_slice())
                .ok()
                .map(|p| p.name);
            client_info.username = username.clone();

            // Kick if lockout is enabled
            if config.lockout.enabled {
                match username {
                    Some(username) => {
                        info!(target: "lazymc", "Kicked '{}' because lockout is enabled", username)
                    }
                    None => info!(target: "lazymc", "Kicked player because lockout is enabled"),
                }
                action::kick(&client, &config.lockout.message, &mut writer).await?;
                break;
            }

            // Kick if client is banned
            if let Some(ban) = server.ban_entry(&client.peer.ip()).await {
                if ban.is_banned() {
                    warn!(target: "lazymc", "Login from banned IP {} ({}), disconnecting", client.peer.ip(), &ban.reason);
                    action::kick(&client, &ban.reason, &mut writer).await?;
                    break;
                }
            }

            // Start server if not starting yet
            Server::start(config.clone(), server.clone(), username).await;

            // Remember inbound packets
            inbound_history.extend(&raw);
            inbound_history.extend(&buf);

            // Build inbound packet queue with everything from login start (including this)
            let mut login_queue = BytesMut::with_capacity(raw.len() + buf.len());
            login_queue.extend(&raw);
            login_queue.extend(&buf);

            // Buf is fully consumed here
            buf.clear();

            // Start occupying client
            join::occupy(
                client,
                client_info,
                config,
                server,
                inbound,
                inbound_history,
                login_queue,
            )
            .await?;
            return Ok(());
        }

        // Show unhandled packet warning
        debug!(target: "lazymc", "Received unhandled packet:");
        debug!(target: "lazymc", "- State: {:?}", client_state);
        debug!(target: "lazymc", "- Packet ID: {}", packet.id);
    }

    Ok(())
}

/// Build server status object to respond to client with.
async fn server_status(config: &Config, server: &Server) -> ServerStatus {
    let status = server.status().await;

    // Select version and player max from last known server status
    let (version, max) = match status.as_ref() {
        Some(status) => (status.version.clone(), status.players.max),
        None => (
            ServerVersion {
                name: config.public.version.clone(),
                protocol: config.public.protocol,
            },
            0,
        ),
    };

    // Select description, use server MOTD if enabled, or use configured
    let description = {
        if config.motd.from_server && status.is_some() {
            status.as_ref().unwrap().description.clone()
        } else {
            Message::new(Payload::text(match server.state() {
                server::State::Stopped | server::State::Started => &config.motd.sleeping,
                server::State::Starting => &config.motd.starting,
                server::State::Stopping => &config.motd.stopping,
            }))
        }
    };

    // Build status resposne
    ServerStatus {
        version,
        description,
        players: OnlinePlayers {
            online: 0,
            max,
            sample: vec![],
        },
    }
}
