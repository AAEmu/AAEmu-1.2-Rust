use aaemu_commons::PacketStream;
use aaemu_game::opcodes::{cs, sc};
use anyhow::bail;

use super::{Packet, LEVEL_GAME};
use crate::record::Dir;

/// The transport has a u16 frame length. Batches may compress at most 1 MiB into that
/// frame and contain at most 4096 entries (enough for the native all-skill cooldown reset).
/// Every expanded entry must also fit a normal level-1 frame. Limits apply before copies
/// and before inflater output growth, not just when events reach the recorder.
pub const MAX_GAME_FRAME: usize = u16::MAX as usize;
pub const MAX_BATCH_BYTES: usize = 1024 * 1024;
pub const MAX_BATCH_PACKETS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BatchError {
    Inflate,
    Limit,
    Structure,
    Unsupported,
}

impl std::fmt::Display for BatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Inflate => "invalid DD04 compression",
            Self::Limit => "DD04 limit exceeded",
            Self::Structure => "invalid DD04 structure",
            Self::Unsupported => "unsupported DD04 opcode",
        })
    }
}
impl std::error::Error for BatchError {}

/// miniz_oxide 0.8.9 core::decompress writes only into the supplied slice and returns
/// (status, input consumed, output written). Unlike DecompressError, status contains no
/// partial plaintext. A fixed 1 MiB output slice bounds allocation even for a tiny bomb.
/// Requiring all input consumed also rejects trailing or concatenated deflate streams.
pub(super) fn inflate_batch(input: &[u8]) -> Result<Vec<u8>, BatchError> {
    use miniz_oxide::inflate::core::{decompress, inflate_flags, DecompressorOxide};
    use miniz_oxide::inflate::TINFLStatus;
    if input.len() > MAX_GAME_FRAME - 4 {
        return Err(BatchError::Limit);
    }
    let mut output = vec![0; MAX_BATCH_BYTES];
    let mut inflater = Box::<DecompressorOxide>::default();
    let (status, used, written) = decompress(
        &mut inflater,
        input,
        &mut output,
        0,
        inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
    );
    match status {
        TINFLStatus::Done if used == input.len() => {
            output.truncate(written);
            Ok(output)
        }
        TINFLStatus::HasMoreOutput => Err(BatchError::Limit),
        _ => Err(BatchError::Inflate),
    }
}

fn batch_caster(s: &mut PacketStream) -> anyhow::Result<()> {
    let kind = s.read_u8()?;
    aaemu_game::systems::skills::types::SkillCaster::read(kind, s)?;
    Ok(())
}

fn batch_cast_action(s: &mut PacketStream) -> anyhow::Result<()> {
    // CastAction has a native writer but no reader (types.rs, CastAction.Write).
    let len = match s.read_u8()? {
        0 => 6,
        1 => 14,
        2 => 13,
        3 => 11,
        4 => 7,
        _ => bail!(BatchError::Structure),
    };
    s.read_bytes(len)?;
    Ok(())
}

fn batch_plot_object(s: &mut PacketStream) -> anyhow::Result<()> {
    // PlotObject.Write: Unit = type1+BC; Position = type2+position9+rotation3.
    let len = match s.read_u8()? {
        1 => 3,
        2 => 12,
        _ => bail!(BatchError::Structure),
    };
    s.read_bytes(len)?;
    Ok(())
}

/// Consume exactly one native body. No boundary scanning or treating remaining bytes as
/// an unknown body: an unsupported opcode rejects the *whole* batch before publication.
/// Covers every current CompressedGamePackets producer: plots, damage/heal and reset_all.
fn batch_body(dir: Dir, opcode: u16, s: &mut PacketStream) -> anyhow::Result<()> {
    if dir == Dir::C2S {
        if opcode == cs::X2EnterWorldPacket {
            s.read_bytes(29)?;
            return Ok(());
        }
        bail!(BatchError::Unsupported);
    }
    match opcode {
        sc::SCReconnectAuthPacket => {
            s.read_u32()?;
        }
        sc::X2EnterWorldResponsePacket => {
            s.read_bytes(17)?; // reason, gm, stream token, port, world time
            s.read_bytes_sized()?; // pubKeySize / pubKey (both native u16-sized)
            s.read_string()?;
        }
        sc::SCSkillCooldownResetPacket => {
            s.read_bytes(12)?;
        }
        sc::SCSkillEndedPacket | sc::SCPlotEndedPacket => {
            s.read_bytes(2)?;
        }
        sc::SCErrorMsgPacket => {
            s.read_bytes(9)?;
        }
        sc::SCCastingStoppedPacket => {
            s.read_bytes(6)?;
        }
        sc::SCBuffRemovedPacket | sc::SCBuffLearnedPacket | sc::SCSkillStoppedPacket => {
            s.read_bytes(7)?;
        }
        sc::SCUnitDamagedPacket => {
            batch_cast_action(s)?;
            batch_caster(s)?;
            s.read_bytes(7)?; // casterBC, targetBC, crime
            s.read_pisc_w(3)?;
            s.read_pisc_w(3)?;
            s.read_bytes(5)?; // holdable, hit type, flag, result
        }
        sc::SCUnitHealedPacket => {
            batch_cast_action(s)?;
            batch_caster(s)?;
            s.read_bytes(14)?; // targetBC, heal/hit, value, zero, result
        }
        sc::SCPlotEventPacket => {
            s.read_bytes(10)?; // tl, event, skill
            batch_plot_object(s)?;
            batch_plot_object(s)?;
            s.read_bytes(18)?; // item, objectBC, casting, zeroBC, zeroi16
            let count = s.read_u8()?;
            s.read_bytes(usize::from(count) * 3)?;
            let flag = s.read_u8()?;
            if flag & 8 != 0 {
                s.read_bytes(13 * 4)?;
            }
        }
        sc::SCBuffCreatedPacket => {
            batch_caster(s)?;
            s.read_bytes(22)?; // caster, ownerBC, index, buff, level, ability, skill
            s.read_pisc_w(4)?;
        }
        sc::SCSkillStartedPacket | sc::SCSkillFiredPacket => {
            use aaemu_game::systems::skills::types::{SkillCastTarget, SkillObject};
            s.read_bytes(6)?; // skill and transport ids
            batch_caster(s)?;
            let kind = s.read_u8()?;
            SkillCastTarget::read(kind, s)?;
            let flag = s.read_u8()?;
            anyhow::ensure!(flag <= 7, "invalid skill object type");
            SkillObject::read(flag, s)?;
            if opcode == sc::SCSkillFiredPacket {
                s.read_bytes(10)?;
            } else {
                s.read_bytes(5)?; // real/base cast, synergy
                let extra = s.read_u8()?;
                anyhow::ensure!(extra & !7 == 0, "invalid skill extra flags");
                if extra & 1 != 0 {
                    s.read_u8()?;
                }
                if extra & 2 != 0 {
                    s.read_u16()?;
                }
                if extra & 4 != 0 {
                    s.read_u32()?;
                }
            }
        }
        _ => bail!(BatchError::Unsupported),
    }
    Ok(())
}

/// Shared native batch validation for GameClient and Recorder. The input is the already
/// bounded inflated body, whose entries are [u16 zero][u16 opcode][native body]. No nesting
/// exists in this format. Any nonzero header, bad count/trailing bytes, or unknown body is
/// rejected atomically with a fixed error, never with input bytes or an underlying cause.
pub(crate) fn split_game_batch(
    dir: Dir,
    count: u16,
    body: &[u8],
) -> Result<Vec<Packet>, BatchError> {
    if body.len() > MAX_BATCH_BYTES || usize::from(count) > MAX_BATCH_PACKETS {
        return Err(BatchError::Limit);
    }
    let mut stream = PacketStream::from_bytes(body.to_vec());
    let mut packets = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        if stream.read_u16().map_err(|_| BatchError::Structure)? != 0 {
            return Err(BatchError::Structure);
        }
        let opcode = stream.read_u16().map_err(|_| BatchError::Structure)?;
        let start = stream.pos();
        if let Err(error) = batch_body(dir, opcode, &mut stream) {
            return Err(
                if error.downcast_ref::<BatchError>() == Some(&BatchError::Unsupported) {
                    BatchError::Unsupported
                } else {
                    BatchError::Structure
                },
            );
        }
        let end = stream.pos();
        if end - start > MAX_GAME_FRAME - 6 {
            return Err(BatchError::Limit);
        }
        packets.push(Packet {
            level: LEVEL_GAME,
            opcode,
            body: body[start..end].to_vec(),
        });
    }
    if stream.remaining() != 0 {
        return Err(BatchError::Structure);
    }
    Ok(packets)
}
