use crate::{
    bits::{BitArray, Serializable},
    error::Error,
    ff::Field,
    helpers::Role,
    protocol::{
        attribution::{
            accumulate_credit::accumulate_credit, aggregate_credit::aggregate_credit,
            credit_capping::credit_capping, AggregateCreditOutputRow, AttributionInputRow,
        },
        boolean::bitwise_equal::bitwise_equal,
        context::{Context, SemiHonestContext},
        modulus_conversion::{convert_all_bits, convert_all_bits_local, transpose},
        sort::{
            apply_sort::{apply_sort_permutation, shuffle::Resharable},
            generate_permutation::generate_permutation_and_reveal_shuffled,
        },
        RecordId, Substep,
    },
    secret_sharing::replicated::semi_honest::{
        AdditiveShare as Replicated, XorShare as XorReplicated,
    },
};
use async_trait::async_trait;
use futures::future::{try_join, try_join_all};
use std::io;
use std::iter::{repeat, zip};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Step {
    ModulusConversionForMatchKeys,
    GenSortPermutationFromMatchKeys,
    ApplySortPermutation,
    ComputeHelperBits,
    AccumulateCredit,
    PerformUserCapping,
    AggregateCredit,
}

impl Substep for Step {}

impl AsRef<str> for Step {
    fn as_ref(&self) -> &str {
        match self {
            Self::ModulusConversionForMatchKeys => "mod_conv_match_key",
            Self::GenSortPermutationFromMatchKeys => "gen_sort_permutation_from_match_keys",
            Self::ApplySortPermutation => "apply_sort_permutation",
            Self::ComputeHelperBits => "compute_helper_bits",
            Self::AccumulateCredit => "accumulate_credit",
            Self::PerformUserCapping => "user_capping",
            Self::AggregateCredit => "aggregate_credit",
        }
    }
}

pub enum IPAInputRowResharableStep {
    MatchKeyShares,
    TriggerBit,
    BreakdownKey,
    TriggerValue,
}

impl Substep for IPAInputRowResharableStep {}

impl AsRef<str> for IPAInputRowResharableStep {
    fn as_ref(&self) -> &str {
        match self {
            Self::MatchKeyShares => "match_key_shares",
            Self::TriggerBit => "is_trigger_bit",
            Self::BreakdownKey => "breakdown_key",
            Self::TriggerValue => "trigger_value",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(Clone))]
pub struct IPAInputRow<F: Field, B: BitArray> {
    pub mk_shares: XorReplicated<B>,
    pub is_trigger_bit: Replicated<F>,
    pub breakdown_key: Replicated<F>,
    pub trigger_value: Replicated<F>,
}

impl<F: Field, B: BitArray> IPAInputRow<F, B> {
    /// Splits the given slice into chunks aligned with the size of this struct and returns an
    /// iterator that produces deserialized instances.
    ///
    /// ## Panics
    /// Panics if the slice buffer is not aligned with the size of this struct.
    pub fn from_byte_slice(input: &[u8]) -> impl Iterator<Item = Self> + '_ {
        assert_eq!(0, input.len() % Self::SIZE_IN_BYTES, "input is not aligned");

        input.chunks(Self::SIZE_IN_BYTES).map(|chunk| {
            let mk_shares = XorReplicated::<B>::deserialize(chunk).unwrap();
            let is_trigger_bit =
                Replicated::<F>::deserialize(&chunk[XorReplicated::<B>::SIZE_IN_BYTES..]).unwrap();
            let breakdown_key = Replicated::<F>::deserialize(
                &chunk[XorReplicated::<B>::SIZE_IN_BYTES + Replicated::<F>::SIZE_IN_BYTES..],
            )
            .unwrap();
            let trigger_value = Replicated::<F>::deserialize(
                &chunk[XorReplicated::<B>::SIZE_IN_BYTES + 2 * Replicated::<F>::SIZE_IN_BYTES..],
            )
            .unwrap();

            Self {
                mk_shares,
                is_trigger_bit,
                breakdown_key,
                trigger_value,
            }
        })
    }
}

impl<F: Field, B: BitArray> Serializable for IPAInputRow<F, B> {
    const SIZE_IN_BYTES: usize =
        3 * Replicated::<F>::SIZE_IN_BYTES + XorReplicated::<B>::SIZE_IN_BYTES;

    fn serialize(self, buf: &mut [u8]) -> io::Result<()> {
        self.mk_shares.serialize(buf)?;
        self.is_trigger_bit
            .serialize(&mut buf[XorReplicated::<B>::SIZE_IN_BYTES..])?;
        self.breakdown_key.serialize(
            &mut buf[XorReplicated::<B>::SIZE_IN_BYTES + Replicated::<F>::SIZE_IN_BYTES..],
        )?;
        self.trigger_value.serialize(
            &mut buf[XorReplicated::<B>::SIZE_IN_BYTES + 2 * Replicated::<F>::SIZE_IN_BYTES..],
        )?;

        Ok(())
    }

    fn deserialize(_buf: &[u8]) -> io::Result<Self> {
        todo!()
    }
}

struct IPAModulusConvertedInputRow<F: Field> {
    mk_shares: Vec<Replicated<F>>,
    is_trigger_bit: Replicated<F>,
    breakdown_key: Replicated<F>,
    trigger_value: Replicated<F>,
}

#[async_trait]
impl<F: Field + Sized> Resharable<F> for IPAModulusConvertedInputRow<F> {
    type Share = Replicated<F>;

    async fn reshare<C>(&self, ctx: C, record_id: RecordId, to_helper: Role) -> Result<Self, Error>
    where
        C: Context<F, Share = <Self as Resharable<F>>::Share> + Send,
    {
        let f_mk_shares = self.mk_shares.reshare(
            ctx.narrow(&IPAInputRowResharableStep::MatchKeyShares),
            record_id,
            to_helper,
        );
        let f_is_trigger_bit = ctx.narrow(&IPAInputRowResharableStep::TriggerBit).reshare(
            &self.is_trigger_bit,
            record_id,
            to_helper,
        );
        let f_breakdown_key = ctx
            .narrow(&IPAInputRowResharableStep::BreakdownKey)
            .reshare(&self.breakdown_key, record_id, to_helper);
        let f_trigger_value = ctx
            .narrow(&IPAInputRowResharableStep::TriggerValue)
            .reshare(&self.trigger_value, record_id, to_helper);

        let (mk_shares, mut outputs) = try_join(
            f_mk_shares,
            try_join_all([f_is_trigger_bit, f_breakdown_key, f_trigger_value]),
        )
        .await?;

        Ok(IPAModulusConvertedInputRow {
            mk_shares,
            is_trigger_bit: outputs.remove(0),
            breakdown_key: outputs.remove(0),
            trigger_value: outputs.remove(0),
        })
    }
}

/// # Errors
/// Propagates errors from multiplications
/// # Panics
/// Propagates errors from multiplications
pub async fn ipa<F: Field, B: BitArray>(
    ctx: SemiHonestContext<'_, F>,
    input_rows: &[IPAInputRow<F, B>],
    per_user_credit_cap: u32,
    max_breakdown_key: u128,
    num_multi_bits: u32,
) -> Result<Vec<AggregateCreditOutputRow<F>>, Error> {
    let mk_shares = input_rows
        .iter()
        .map(|x| x.mk_shares.clone())
        .collect::<Vec<_>>();
    let local_lists = convert_all_bits_local(ctx.role(), &mk_shares);
    let converted_shares = convert_all_bits(
        &ctx.narrow(&Step::ModulusConversionForMatchKeys),
        &local_lists,
    )
    .await
    .unwrap();
    let sort_permutation = generate_permutation_and_reveal_shuffled(
        ctx.narrow(&Step::GenSortPermutationFromMatchKeys),
        &converted_shares,
        B::BITS,
        num_multi_bits,
    )
    .await
    .unwrap();
    let converted_shares = transpose(&converted_shares);

    let combined_match_keys_and_sidecar_data = input_rows
        .iter()
        .zip(converted_shares.into_iter())
        .map(|(input_row, mk_shares)| IPAModulusConvertedInputRow {
            mk_shares,
            is_trigger_bit: input_row.is_trigger_bit.clone(),
            breakdown_key: input_row.breakdown_key.clone(),
            trigger_value: input_row.trigger_value.clone(),
        })
        .collect::<Vec<_>>();

    let sorted_rows = apply_sort_permutation(
        ctx.narrow(&Step::ApplySortPermutation),
        combined_match_keys_and_sidecar_data,
        &sort_permutation,
    )
    .await
    .unwrap();

    let futures = zip(
        repeat(ctx.narrow(&Step::ComputeHelperBits)),
        sorted_rows.iter(),
    )
    .zip(sorted_rows.iter().skip(1))
    .enumerate()
    .map(|(i, ((ctx, row), next_row))| {
        let record_id = RecordId::from(i);
        async move { bitwise_equal(ctx, record_id, &row.mk_shares, &next_row.mk_shares).await }
    });
    let helper_bits = Some(Replicated::ZERO)
        .into_iter()
        .chain(try_join_all(futures).await?);

    let attribution_input_rows = zip(sorted_rows, helper_bits)
        .map(|(row, hb)| AttributionInputRow {
            is_trigger_bit: row.is_trigger_bit,
            helper_bit: hb,
            breakdown_key: row.breakdown_key,
            credit: row.trigger_value,
        })
        .collect::<Vec<_>>();

    let accumulated_credits =
        accumulate_credit(ctx.narrow(&Step::AccumulateCredit), &attribution_input_rows).await?;

    let user_capped_credits = credit_capping(
        ctx.narrow(&Step::PerformUserCapping),
        &accumulated_credits,
        per_user_credit_cap,
    )
    .await?;

    aggregate_credit(
        ctx.narrow(&Step::AggregateCredit),
        &user_capped_credits,
        max_breakdown_key,
        num_multi_bits,
    )
    .await
}

#[cfg(all(any(test, feature = "test-fixture"), not(feature = "shuttle")))]
pub mod test_cases {
    use super::*;
    use crate::rand::Rng;
    use crate::secret_sharing::IntoShares;
    use crate::test_fixture::Reconstruct;
    use rand::distributions::{Distribution, Standard};
    use std::marker::PhantomData;

    /// The simplest input for IPA circuit that can validate the correctness of the protocol.
    pub struct Simple<F, B> {
        records: Vec<crate::test_fixture::ipa_input_row::IPAInputTestRow>,
        phantom: PhantomData<(F, B)>,
    }

    impl<F: Field, B: BitArray> IntoShares<Vec<IPAInputRow<F, B>>> for Simple<F, B>
    where
        Standard: Distribution<F> + Distribution<B>,
    {
        fn share_with<R: Rng>(self, _rng: &mut R) -> [Vec<IPAInputRow<F, B>>; 3] {
            self.records.share()
        }
    }

    impl<F: Field, B> Simple<F, B> {
        pub const PER_USER_CAP: u32 = 3;
        pub const EXPECTED: &'static [[u128; 2]] = &[[0, 0], [1, 2], [2, 3]];
        pub const MAX_BREAKDOWN_KEY: u128 = 3;

        #[allow(clippy::missing_panics_doc)]
        pub fn validate(results: &[Vec<AggregateCreditOutputRow<F>>; 3]) {
            let results = results.reconstruct();
            assert_eq!(Self::EXPECTED.len(), results.len());

            for (i, expected) in Self::EXPECTED.iter().enumerate() {
                // Each element in the `result` is a general purpose `[F; 4]`.
                // For this test case, the first two elements are `breakdown_key`
                // and `credit` as defined by the implementation of `Reconstruct`
                // for `[AggregateCreditOutputRow<F>; 3]`.
                let result = results[i].0.map(|x| x.as_u128());
                assert_eq!(*expected, [result[0], result[1]]);
            }
        }
    }

    impl<F, B> Default for Simple<F, B> {
        fn default() -> Self {
            use crate::test_fixture::ipa_input_row::IPAInputTestRow;

            // match key, is_trigger, breakdown_key, trigger_value
            let records = vec![
                IPAInputTestRow {
                    match_key: 12345,
                    is_trigger_bit: 0,
                    breakdown_key: 1,
                    trigger_value: 0,
                },
                IPAInputTestRow {
                    match_key: 12345,
                    is_trigger_bit: 0,
                    breakdown_key: 2,
                    trigger_value: 0,
                },
                IPAInputTestRow {
                    match_key: 68362,
                    is_trigger_bit: 0,
                    breakdown_key: 1,
                    trigger_value: 0,
                },
                IPAInputTestRow {
                    match_key: 12345,
                    is_trigger_bit: 1,
                    breakdown_key: 0,
                    trigger_value: 5,
                },
                IPAInputTestRow {
                    match_key: 68362,
                    is_trigger_bit: 1,
                    breakdown_key: 0,
                    trigger_value: 2,
                },
            ];

            Self {
                records,
                phantom: PhantomData::default(),
            }
        }
    }
}

#[cfg(all(test, not(feature = "shuttle")))]
pub mod tests {
    use crate::{
        bits::Serializable,
        ff::{Fp31, Fp32BitPrime},
        protocol::{
            ipa::{ipa, test_cases::Simple, IPAInputRow},
            MatchKey,
        },
        rand::thread_rng,
        secret_sharing::IntoShares,
        test_fixture::{ipa_input_row::IPAInputTestRow, Reconstruct, Runner, TestWorld},
    };
    use proptest::{
        proptest,
        test_runner::{RngAlgorithm, TestRng},
    };

    #[tokio::test]
    #[allow(clippy::missing_panics_doc)]
    pub async fn semi_honest() {
        const COUNT: usize = 5;
        const PER_USER_CAP: u32 = 3;
        const EXPECTED: &[[u128; 2]] = &[[0, 0], [1, 2], [2, 3]];
        const MAX_BREAKDOWN_KEY: u128 = 3;
        const NUM_MULTI_BITS: u32 = 3;

        type SimpleTestCase = Simple<Fp31, MatchKey>;

        let world = TestWorld::new().await;
        let records = SimpleTestCase::default();

        let result = world
            .semi_honest(records, |ctx, input_rows| async move {
                ipa::<Fp31, MatchKey>(
                    ctx,
                    &input_rows,
                    PER_USER_CAP,
                    MAX_BREAKDOWN_KEY,
                    NUM_MULTI_BITS,
                )
                .await
                .unwrap()
            })
            .await;

        SimpleTestCase::validate(&result);
    }

    #[tokio::test]
    #[allow(clippy::missing_panics_doc)]
    #[ignore]
    pub async fn random_ipa_no_result_check() {
        const BATCHSIZE: u64 = 20;
        const PER_USER_CAP: u32 = 10;
        const MAX_BREAKDOWN_KEY: u128 = 8;
        const MAX_TRIGGER_VALUE: u128 = 5;
        const NUM_MULTI_BITS: u32 = 3;

        let max_match_key: u64 = BATCHSIZE / 10;

        let world = TestWorld::new().await;
        let mut rng = thread_rng();

        let mut records: Vec<IPAInputTestRow> = Vec::new();

        for _ in 0..BATCHSIZE {
            records.push(IPAInputTestRow::random(
                &mut rng,
                max_match_key,
                MAX_BREAKDOWN_KEY,
                MAX_TRIGGER_VALUE,
            ));
        }
        let result = world
            .semi_honest(records, |ctx, input_rows| async move {
                ipa::<Fp32BitPrime, MatchKey>(
                    ctx,
                    &input_rows,
                    PER_USER_CAP,
                    MAX_BREAKDOWN_KEY,
                    NUM_MULTI_BITS,
                )
                .await
                .unwrap()
            })
            .await
            .reconstruct();

        assert_eq!(MAX_BREAKDOWN_KEY, result.len() as u128);
    }

    fn serde_internal(
        match_key: u64,
        trigger_bit: u128,
        breakdown_key: u128,
        trigger_value: u128,
        seed: u128,
    ) {
        type RowType = IPAInputRow<Fp31, MatchKey>;
        // xorshift requires 16 byte seed and that's why it is picked here
        let mut rng = TestRng::from_seed(RngAlgorithm::XorShift, &seed.to_le_bytes());
        let [a, b, ..]: [RowType; 3] = IPAInputTestRow {
            match_key,
            is_trigger_bit: trigger_bit,
            breakdown_key,
            trigger_value,
        }
        .share_with(&mut rng);

        let mut buf = vec![0u8; 2 * RowType::SIZE_IN_BYTES];
        a.clone().serialize(&mut buf).unwrap();
        b.clone()
            .serialize(&mut buf[RowType::SIZE_IN_BYTES..])
            .unwrap();

        assert_eq!(
            vec![a, b],
            RowType::from_byte_slice(&buf).collect::<Vec<_>>()
        );
    }

    proptest! {
        #[test]
        fn serde(match_key in 0..u64::MAX, trigger_bit in 0..u128::MAX, breakdown_key in 0..u128::MAX, trigger_value in 0..u128::MAX, seed in 0..u128::MAX) {
            serde_internal(match_key, trigger_bit, breakdown_key, trigger_value, seed);
        }
    }
}
