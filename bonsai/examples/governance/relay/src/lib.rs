// Copyright 2023 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bonsai_sdk::alpha::{responses::SnarkProof, Client, SdkErr};
use clap::{builder::PossibleValue, ValueEnum};
use risc0_build::GuestListEntry;
use risc0_zkvm::{
    ExecutorEnv, Executor, MemoryImage, Program, ReceiptMetadata, Receipt,
    MEM_SIZE, PAGE_SIZE,
};

#[derive(Debug, Copy, Clone)]
pub enum ProverMode {
    None,
    Local,
    Bonsai,
}

impl ValueEnum for ProverMode {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::None, Self::Local, Self::Bonsai]
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(match self {
            Self::None => PossibleValue::new("none"),
            Self::Local => PossibleValue::new("local"),
            Self::Bonsai => PossibleValue::new("bonsai"),
        })
    }
}

/// Result of executing a guest image, possibly containing a proof.
pub enum Output {
    Execution {
        journal: Vec<u8>,
    },
    Bonsai {
        journal: Vec<u8>,
        receipt_metadata: ReceiptMetadata,
        snark_proof: SnarkProof,
    },
}

/// Execute and prove the guest locally, on this machine, as opposed to sending
/// the proof request to the Bonsai service.
pub fn prove_locally(elf: &[u8], input: Vec<u8>, prove: bool) -> Result<Output> {
    // Execute the guest program, generating the session trace needed to prove the
    // computation.
    let env = ExecutorEnv::builder()
        .add_input(&input)
        .build()
        .context("Failed to build exec env")?;
    let mut exec = Executor::from_elf(env, elf).context("Failed to instantiate executor")?;
    let session = exec
        .run()
        .context(format!("Failed to run executor {:?}", &input))?;

    // Locally prove resulting journal
    if prove {
        session.prove().context("Failed to prove session")?;
        // eprintln!("Completed proof locally");
    } else {
        // eprintln!("Completed execution without a proof locally");
    }
    Ok(Output::Execution {
        journal: session.journal,
    })
}

pub const POLL_INTERVAL_SEC: u64 = 4;

fn get_digest(elf: &[u8]) -> Result<String> {
    let program = Program::load_elf(elf, MEM_SIZE as u32)?;
    let image = MemoryImage::new(&program, PAGE_SIZE as u32)?;
    Ok(hex::encode(image.compute_id()))
}

pub fn prove_alpha(elf: &[u8], input: Vec<u8>) -> Result<Output> {
    let client = Client::from_env().context("Failed to create client from env var")?;

    let img_id = get_digest(elf).context("Failed to generate elf memory image")?;

    match client.upload_img(&img_id, elf.to_vec()) {
        Ok(()) => (),
        Err(SdkErr::ImageIdExists) => (),
        Err(err) => return Err(err.into()),
    }

    let input_id = client
        .upload_input(input)
        .context("Failed to upload input data")?;

    let session = client
        .create_session(img_id, input_id)
        .context("Failed to create remote proving session")?;

    // Poll and await the result of the STARK rollup proving session.
    let receipt: Receipt = (|| {
        loop {
            let res = match session.status(&client) {
                Ok(res) => res,
                Err(err) => {
                    eprint!("Failed to get session status: {err}");
                    std::thread::sleep(Duration::from_secs(POLL_INTERVAL_SEC));
                    continue;
                }
            };
            match res.status.as_str() {
                "RUNNING" => {
                    std::thread::sleep(Duration::from_secs(POLL_INTERVAL_SEC));
                }
                "SUCCEEDED" => {
                    let receipt_buf = client
                        .download(
                            &res.receipt_url
                                .context("Missing 'receipt_url' on status response")?,
                        )
                        .context("Failed to download receipt")?;
                    let receipt: Receipt = bincode::deserialize(&receipt_buf)
                        .context("Failed to deserialize Receipt")?;
                    // eprintln!("Completed STARK proof on bonsai alpha backend!");
                    return Ok(receipt);
                }
                _ => {
                    bail!(
                        "STARK proving session exited with bad status: {}",
                        res.status
                    );
                }
            }
        }
    })()?;
    let metadata = receipt
        .inner
        .flat()
        .last()
        .ok_or(anyhow!("receipt contains no segments"))?
        .get_metadata()?;

    let snark_session = client.create_snark(session.uuid)?;
    let snark_proof: SnarkProof = (|| loop {
        let res = snark_session.status(&client)?;
        match res.status.as_str() {
            "RUNNING" => {
                std::thread::sleep(Duration::from_secs(POLL_INTERVAL_SEC));
            }
            "SUCCEEDED" => {
                // eprintln!("Completed SNARK proof on bonsai alpha backend!");
                return res
                    .output
                    .ok_or(anyhow!("output expected to be non-empty on success"));
            }
            _ => {
                bail!(
                    "SNARK proving session exited with bad status: {}",
                    res.status
                );
            }
        }
    })()?;

    Ok(Output::Bonsai {
        journal: receipt.journal,
        receipt_metadata: metadata,
        snark_proof,
    })
}

pub fn resolve_guest_entry<'a>(
    guest_list: &'a [GuestListEntry],
    guest_binary: &String,
) -> Result<&'a GuestListEntry> {
    // Search list for requested binary name
    let potential_guest_image_id: [u8; 32] =
        match hex::decode(guest_binary.to_lowercase().trim_start_matches("0x")) {
            Ok(byte_vector) => byte_vector.try_into().unwrap_or([0u8; 32]),
            Err(_) => [0u8; 32],
        };
    guest_list
        .iter()
        .find(|entry| {
            entry.name == guest_binary.to_uppercase()
                || bytemuck::cast::<[u32; 8], [u8; 32]>(entry.image_id) == potential_guest_image_id
        })
        .ok_or_else(|| {
            let found_guests: Vec<String> = guest_list
                .iter()
                .map(|g| hex::encode(bytemuck::cast::<[u32; 8], [u8; 32]>(g.image_id)))
                .collect();
            anyhow!(
                "Unknown guest binary {}, found: {:?}",
                guest_binary,
                found_guests
            )
        })
}

pub async fn resolve_image_output(
    input: &str,
    guest_entry: &GuestListEntry,
    prover_mode: ProverMode,
) -> Result<Output> {
    let input = hex::decode(input.trim_start_matches("0x")).context("Failed to decode input")?;
    let elf = guest_entry.elf;

    match prover_mode {
        ProverMode::Bonsai => tokio::task::spawn_blocking(move || prove_alpha(elf, input))
            .await
            .context("Failed to run alpha sub-task")?,
        ProverMode::Local => prove_locally(elf, input, true),
        ProverMode::None => prove_locally(elf, input, false),
    }
}
