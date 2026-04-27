//! Paired account signer and signer-sponsorship descriptor management.

use std::cmp::Ordering;

use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1Ext, AccountId, Signer, SignerKey,
    SponsorshipDescriptor, VecM,
};

use crate::{Result, TxError};

use super::ensure_account_ext_v2;

#[derive(Clone)]
struct SignerSlot {
    signer: Signer,
    sponsor: Option<AccountId>,
}

#[derive(Clone)]
enum SignerSetInner {
    Untracked(Vec<Signer>),
    Tracked(Vec<SignerSlot>),
}

/// Owned account signer view that keeps signer sponsorship descriptors paired
/// with their signer while callers mutate, sort, and write signers back.
#[derive(Clone)]
pub struct SignerSet {
    inner: SignerSetInner,
}

impl SignerSet {
    /// Build a strict signer view. If the account has ext-v2, Stellar requires
    /// one descriptor per signer.
    pub fn strict_from_account(account: &AccountEntry) -> Result<Self> {
        match &account.ext {
            AccountEntryExt::V1(v1) => match &v1.ext {
                AccountEntryExtensionV1Ext::V2(v2) => {
                    if v2.signer_sponsoring_i_ds.len() != account.signers.len() {
                        return Err(TxError::Internal(format!(
                            "signer descriptor length mismatch: descriptors len {} != signers len {}",
                            v2.signer_sponsoring_i_ds.len(),
                            account.signers.len()
                        )));
                    }
                    let slots = account
                        .signers
                        .iter()
                        .cloned()
                        .zip(v2.signer_sponsoring_i_ds.iter().map(|id| id.0.clone()))
                        .map(|(signer, sponsor)| SignerSlot { signer, sponsor })
                        .collect();
                    Ok(Self {
                        inner: SignerSetInner::Tracked(slots),
                    })
                }
                AccountEntryExtensionV1Ext::V0 => Ok(Self {
                    inner: SignerSetInner::Untracked(account.signers.to_vec()),
                }),
            },
            AccountEntryExt::V0 => Ok(Self {
                inner: SignerSetInner::Untracked(account.signers.to_vec()),
            }),
        }
    }

    /// Build an untracked signer view for set-options updates that do not need
    /// signer sponsorship descriptors.
    pub fn untracked_from_account(account: &AccountEntry) -> Self {
        Self {
            inner: SignerSetInner::Untracked(account.signers.to_vec()),
        }
    }

    /// Build a descriptor-tracked view for set-options. This preserves the
    /// existing set-options behavior of normalizing descriptor length while the
    /// operation rewrites signer state.
    pub fn normalized_for_set_options(account: &AccountEntry) -> Self {
        let mut sponsors = match &account.ext {
            AccountEntryExt::V1(v1) => match &v1.ext {
                AccountEntryExtensionV1Ext::V2(v2) => v2
                    .signer_sponsoring_i_ds
                    .iter()
                    .map(|id| id.0.clone())
                    .collect::<Vec<_>>(),
                AccountEntryExtensionV1Ext::V0 => Vec::new(),
            },
            AccountEntryExt::V0 => Vec::new(),
        };

        if sponsors.len() < account.signers.len() {
            sponsors.extend(std::iter::repeat(None).take(account.signers.len() - sponsors.len()));
        } else if sponsors.len() > account.signers.len() {
            sponsors.truncate(account.signers.len());
        }

        let slots = account
            .signers
            .iter()
            .cloned()
            .zip(sponsors)
            .map(|(signer, sponsor)| SignerSlot { signer, sponsor })
            .collect();

        Self {
            inner: SignerSetInner::Tracked(slots),
        }
    }

    pub fn len(&self) -> usize {
        match &self.inner {
            SignerSetInner::Untracked(signers) => signers.len(),
            SignerSetInner::Tracked(slots) => slots.len(),
        }
    }

    pub fn position(&self, key: &SignerKey) -> Option<usize> {
        match &self.inner {
            SignerSetInner::Untracked(signers) => signers.iter().position(|s| &s.key == key),
            SignerSetInner::Tracked(slots) => slots.iter().position(|s| &s.signer.key == key),
        }
    }

    pub fn sponsor_at(&self, index: usize) -> Result<Option<AccountId>> {
        match &self.inner {
            SignerSetInner::Untracked(signers) => {
                if index >= signers.len() {
                    return Err(index_out_of_bounds(index, signers.len()));
                }
                Ok(None)
            }
            SignerSetInner::Tracked(slots) => slots
                .get(index)
                .map(|slot| slot.sponsor.clone())
                .ok_or_else(|| index_out_of_bounds(index, slots.len())),
        }
    }

    pub fn update_weight(&mut self, index: usize, weight: u32) -> Result<()> {
        match &mut self.inner {
            SignerSetInner::Untracked(signers) => {
                let len = signers.len();
                let signer = signers
                    .get_mut(index)
                    .ok_or_else(|| index_out_of_bounds(index, len))?;
                signer.weight = weight;
            }
            SignerSetInner::Tracked(slots) => {
                let len = slots.len();
                let slot = slots
                    .get_mut(index)
                    .ok_or_else(|| index_out_of_bounds(index, len))?;
                slot.signer.weight = weight;
            }
        }
        Ok(())
    }

    pub fn remove(&mut self, index: usize) -> Result<Option<AccountId>> {
        match &mut self.inner {
            SignerSetInner::Untracked(signers) => {
                if index >= signers.len() {
                    return Err(index_out_of_bounds(index, signers.len()));
                }
                signers.remove(index);
                Ok(None)
            }
            SignerSetInner::Tracked(slots) => {
                if index >= slots.len() {
                    return Err(index_out_of_bounds(index, slots.len()));
                }
                Ok(slots.remove(index).sponsor)
            }
        }
    }

    pub fn set_sponsor(&mut self, index: usize, sponsor: Option<AccountId>) -> Result<()> {
        match &mut self.inner {
            SignerSetInner::Untracked(_) => Err(TxError::Internal(
                "cannot set signer sponsor without descriptor tracking".to_string(),
            )),
            SignerSetInner::Tracked(slots) => {
                let len = slots.len();
                let slot = slots
                    .get_mut(index)
                    .ok_or_else(|| index_out_of_bounds(index, len))?;
                slot.sponsor = sponsor;
                Ok(())
            }
        }
    }

    pub fn push(&mut self, signer: Signer, sponsor: Option<AccountId>) -> Result<()> {
        match &mut self.inner {
            SignerSetInner::Untracked(signers) => signers.push(signer),
            SignerSetInner::Tracked(slots) => slots.push(SignerSlot { signer, sponsor }),
        }
        Ok(())
    }

    pub fn sort_by_signer_key(&mut self) {
        match &mut self.inner {
            SignerSetInner::Untracked(signers) => {
                signers.sort_by(|a, b| compare_signer_keys(&a.key, &b.key));
            }
            SignerSetInner::Tracked(slots) => {
                slots.sort_by(|a, b| compare_signer_keys(&a.signer.key, &b.signer.key));
            }
        }
    }

    pub fn sponsored_signers(&self) -> Vec<AccountId> {
        match &self.inner {
            SignerSetInner::Untracked(_) => Vec::new(),
            SignerSetInner::Tracked(slots) => slots
                .iter()
                .filter_map(|slot| slot.sponsor.clone())
                .collect(),
        }
    }

    /// Build bounded XDR vectors without mutating the account.
    pub fn prepare_write(&self) -> Result<PreparedSignerWrite> {
        match &self.inner {
            SignerSetInner::Untracked(signers) => Ok(PreparedSignerWrite {
                signers: signers_to_vecm(signers.clone())?,
                descriptors: None,
            }),
            SignerSetInner::Tracked(slots) => {
                let signers = slots
                    .iter()
                    .map(|slot| slot.signer.clone())
                    .collect::<Vec<_>>();
                let descriptors = slots
                    .iter()
                    .map(|slot| SponsorshipDescriptor(slot.sponsor.clone()))
                    .collect::<Vec<_>>();
                Ok(PreparedSignerWrite {
                    signers: signers_to_vecm(signers)?,
                    descriptors: Some(descriptors_to_vecm(descriptors)?),
                })
            }
        }
    }

    pub fn write_to_account(&self, account: &mut AccountEntry) -> Result<()> {
        let prepared = self.prepare_write()?;
        prepared.apply(account);
        Ok(())
    }
}

pub struct PreparedSignerWrite {
    signers: VecM<Signer, 20>,
    descriptors: Option<VecM<SponsorshipDescriptor, 20>>,
}

impl PreparedSignerWrite {
    pub fn apply(self, account: &mut AccountEntry) {
        account.signers = self.signers;
        if let Some(descriptors) = self.descriptors {
            let ext = ensure_account_ext_v2(account);
            ext.signer_sponsoring_i_ds = descriptors;
        }
    }
}

pub fn validate_strict_signer_descriptors(account: &AccountEntry) -> Result<()> {
    let _ = SignerSet::strict_from_account(account)?;
    Ok(())
}

pub fn strict_sponsored_signers(account: &AccountEntry) -> Result<Vec<AccountId>> {
    Ok(SignerSet::strict_from_account(account)?.sponsored_signers())
}

pub fn compare_signer_keys(a: &SignerKey, b: &SignerKey) -> Ordering {
    match (a, b) {
        (SignerKey::Ed25519(a_key), SignerKey::Ed25519(b_key)) => a_key.0.cmp(&b_key.0),
        (SignerKey::PreAuthTx(a_key), SignerKey::PreAuthTx(b_key)) => a_key.0.cmp(&b_key.0),
        (SignerKey::HashX(a_key), SignerKey::HashX(b_key)) => a_key.0.cmp(&b_key.0),
        (SignerKey::Ed25519SignedPayload(a_key), SignerKey::Ed25519SignedPayload(b_key)) => a_key
            .ed25519
            .0
            .cmp(&b_key.ed25519.0)
            .then_with(|| a_key.payload.as_slice().cmp(b_key.payload.as_slice())),
        (a, b) => signer_key_discriminant(a).cmp(&signer_key_discriminant(b)),
    }
}

fn signer_key_discriminant(key: &SignerKey) -> u8 {
    match key {
        SignerKey::Ed25519(_) => 0,
        SignerKey::PreAuthTx(_) => 1,
        SignerKey::HashX(_) => 2,
        SignerKey::Ed25519SignedPayload(_) => 3,
    }
}

fn signers_to_vecm(signers: Vec<Signer>) -> Result<VecM<Signer, 20>> {
    signers
        .try_into()
        .map_err(|_| TxError::Internal("too many account signers".to_string()))
}

fn descriptors_to_vecm(
    descriptors: Vec<SponsorshipDescriptor>,
) -> Result<VecM<SponsorshipDescriptor, 20>> {
    descriptors
        .try_into()
        .map_err(|_| TxError::Internal("too many signer sponsorship descriptors".to_string()))
}

fn index_out_of_bounds(index: usize, len: usize) -> TxError {
    TxError::Internal(format!(
        "signer descriptor index out of bounds: index {} >= len {}",
        index, len
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        AccountEntryExtensionV1, AccountEntryExtensionV2, AccountEntryExtensionV2Ext, Liabilities,
        PublicKey, SignerKeyEd25519SignedPayload, String32, Thresholds, Uint256,
    };

    fn account_with(signers: Vec<Signer>, descriptors: Vec<SponsorshipDescriptor>) -> AccountEntry {
        AccountEntry {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1; 32]))),
            balance: 100,
            seq_num: stellar_xdr::curr::SequenceNumber(1),
            num_sub_entries: signers.len() as u32,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: signers.try_into().unwrap(),
            ext: AccountEntryExt::V1(AccountEntryExtensionV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 0,
                },
                ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                    num_sponsored: 0,
                    num_sponsoring: 0,
                    signer_sponsoring_i_ds: descriptors.try_into().unwrap(),
                    ext: AccountEntryExtensionV2Ext::V0,
                }),
            }),
        }
    }

    fn signer(seed: u8) -> Signer {
        Signer {
            key: SignerKey::Ed25519(Uint256([seed; 32])),
            weight: 1,
        }
    }

    #[test]
    fn strict_rejects_descriptor_length_mismatch() {
        let account = account_with(
            vec![signer(1)],
            vec![
                SponsorshipDescriptor(None),
                SponsorshipDescriptor(Some(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
                    [2; 32],
                ))))),
            ],
        );

        assert!(SignerSet::strict_from_account(&account).is_err());
    }

    #[test]
    fn normalized_set_options_truncates_extra_descriptors_locally() {
        let mut account = account_with(
            vec![signer(1)],
            vec![
                SponsorshipDescriptor(None),
                SponsorshipDescriptor(Some(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
                    [2; 32],
                ))))),
            ],
        );
        let set = SignerSet::normalized_for_set_options(&account);
        set.write_to_account(&mut account).unwrap();

        let AccountEntryExt::V1(v1) = &account.ext else {
            panic!("expected v1");
        };
        let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext else {
            panic!("expected v2");
        };
        assert_eq!(v2.signer_sponsoring_i_ds.len(), 1);
    }

    #[test]
    fn signed_payload_order_includes_payload() {
        let ed25519 = Uint256([5; 32]);
        let key_a = SignerKey::Ed25519SignedPayload(SignerKeyEd25519SignedPayload {
            ed25519: ed25519.clone(),
            payload: vec![1, 2].try_into().unwrap(),
        });
        let key_b = SignerKey::Ed25519SignedPayload(SignerKeyEd25519SignedPayload {
            ed25519,
            payload: vec![1, 3].try_into().unwrap(),
        });

        assert_eq!(compare_signer_keys(&key_a, &key_b), Ordering::Less);
        assert_eq!(compare_signer_keys(&key_b, &key_a), Ordering::Greater);
    }
}
