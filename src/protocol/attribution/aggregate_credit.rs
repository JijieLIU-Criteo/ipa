use super::CappedCreditsWithAggregationBit;
use crate::error::Error;
use crate::ff::{Field, Int};
use crate::helpers::Role;
use crate::protocol::attribution::AttributionResharableStep::{
    AggregationBit, BreakdownKey, Credit, HelperBit,
};
use crate::protocol::boolean::{random_bits_generator::RandomBitsGenerator, BitDecomposition};
use crate::protocol::context::{Context, SemiHonestContext};
use crate::protocol::sort::apply_sort::apply_sort_permutation;
use crate::protocol::sort::apply_sort::shuffle::Resharable;
use crate::protocol::sort::generate_permutation::generate_permutation_and_reveal_shuffled;
use crate::protocol::{RecordId, Substep};
use crate::secret_sharing::Replicated;
use async_trait::async_trait;
use futures::future::try_join_all;
use std::iter::repeat;

#[async_trait]
impl<F: Field> Resharable<F> for CappedCreditsWithAggregationBit<F>
where
    F: Sized,
{
    type Share = Replicated<F>;

    async fn reshare<C>(&self, ctx: C, record_id: RecordId, to_helper: Role) -> Result<Self, Error>
    where
        C: Context<F, Share = <Self as Resharable<F>>::Share> + Send,
    {
        let f_helper_bit = ctx
            .narrow(&HelperBit)
            .reshare(&self.helper_bit, record_id, to_helper);
        let f_aggregation_bit =
            ctx.narrow(&AggregationBit)
                .reshare(&self.aggregation_bit, record_id, to_helper);
        let f_breakdown_key =
            ctx.narrow(&BreakdownKey)
                .reshare(&self.breakdown_key, record_id, to_helper);
        let f_value = ctx
            .narrow(&Credit)
            .reshare(&self.credit, record_id, to_helper);

        let mut outputs =
            try_join_all([f_helper_bit, f_aggregation_bit, f_breakdown_key, f_value]).await?;

        Ok(CappedCreditsWithAggregationBit {
            helper_bit: outputs.remove(0),
            aggregation_bit: outputs.remove(0),
            breakdown_key: outputs.remove(0),
            credit: outputs.remove(0),
        })
    }
}

/// Transpose rows of bits into bits of rows
///
/// input:
/// `[`
/// `[ row[0].bit0, row[0].bit1, ..., row[0].bit31 ]`,
/// `[ row[1].bit0, row[1].bit1, ..., row[1].bit31 ]`,
/// ...
/// `[ row[n].bit0, row[n].bit1, ..., row[n].bit31 ]`,
/// `]`
///
/// output:
/// `[`
/// `[ row[0].bit0, row[1].bit0, ..., row[n].bit0 ]`,
/// `[ row[0].bit1, row[1].bit1, ..., row[n].bit1 ]`,
/// ...
/// `[ row[0].bit31, row[1].bit31, ..., row[n].bit31 ]`,
/// `]`
fn transpose<F: Field>(input: &[Vec<Replicated<F>>]) -> Vec<Vec<Replicated<F>>> {
    let bit_length = input[0].len();
    debug_assert_eq!(bit_length, F::Integer::BITS as usize);

    (0..bit_length)
        .map(|i| input.iter().map(|b| b[i].clone()).collect::<Vec<_>>())
        .collect::<Vec<_>>()
}

async fn bit_decompose_breakdown_key<F: Field>(
    ctx: SemiHonestContext<'_, F>,
    input: &[CappedCreditsWithAggregationBit<F>],
) -> Result<Vec<Vec<Replicated<F>>>, Error> {
    let random_bits_generator = RandomBitsGenerator::new();
    try_join_all(
        input
            .iter()
            .zip(repeat(ctx))
            .enumerate()
            .map(|(i, (x, c))| {
                let rbg = random_bits_generator.clone();
                async move {
                    BitDecomposition::execute(c, RecordId::from(i), rbg, &x.breakdown_key).await
                }
            })
            .collect::<Vec<_>>(),
    )
    .await
}

/// Sort the input by `aggregation_bit` first, then by `breakdown_key`
#[allow(dead_code)]
async fn sort_by_aggregation_bit_and_breakdown_key<F: Field>(
    ctx: SemiHonestContext<'_, F>,
    input: &[CappedCreditsWithAggregationBit<F>],
) -> Result<Vec<CappedCreditsWithAggregationBit<F>>, Error> {
    // Sort by aggregation_bit
    let sorted_by_aggregation_bit = sort_by_aggregation_bit(ctx.clone(), input).await?;

    // Next, sort by breakdown_key
    // TODO: Change breakdown_keys to use XorReplicated to avoid bit-decomposition calls
    let breakdown_keys = transpose(
        &bit_decompose_breakdown_key(
            ctx.narrow(&Step::BitDecomposeBreakdownKey),
            &sorted_by_aggregation_bit,
        )
        .await?,
    );

    let sort_permutation = generate_permutation_and_reveal_shuffled(
        ctx.narrow(&Step::GeneratePermutationByBreakdownKey),
        &breakdown_keys,
        F::Integer::BITS,
    )
    .await?;
    apply_sort_permutation(
        ctx.narrow(&Step::ApplyPermutationOnBreakdownKey),
        sorted_by_aggregation_bit.clone(),
        &sort_permutation,
    )
    .await
}

async fn sort_by_aggregation_bit<F: Field>(
    ctx: SemiHonestContext<'_, F>,
    input: &[CappedCreditsWithAggregationBit<F>],
) -> Result<Vec<CappedCreditsWithAggregationBit<F>>, Error> {
    // Since aggregation_bit is a 1-bit share of 1 or 0, we'll just extract the
    // field and wrap it in another vector.
    let aggregation_bits = &[input
        .iter()
        .map(|x| x.aggregation_bit.clone())
        .collect::<Vec<_>>()];

    let sort_permutation = generate_permutation_and_reveal_shuffled(
        ctx.narrow(&Step::GeneratePermutationByAttributionBit),
        aggregation_bits,
        1,
    )
    .await?;

    apply_sort_permutation(
        ctx.narrow(&Step::ApplyPermutationOnAttributionBit),
        input.to_vec(),
        &sort_permutation,
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Step {
    BitDecomposeBreakdownKey,
    GeneratePermutationByBreakdownKey,
    ApplyPermutationOnBreakdownKey,
    GeneratePermutationByAttributionBit,
    ApplyPermutationOnAttributionBit,
}

impl Substep for Step {}

impl AsRef<str> for Step {
    fn as_ref(&self) -> &str {
        match self {
            Self::BitDecomposeBreakdownKey => "bit_decompose_breakdown_key",
            Self::GeneratePermutationByBreakdownKey => "generate_permutation_by_breakdown_key",
            Self::ApplyPermutationOnBreakdownKey => "apply_permutation_by_breakdown_key",
            Self::GeneratePermutationByAttributionBit => "apply_permutation_by_attribution_bit",
            Self::ApplyPermutationOnAttributionBit => "apply_permutation_on_attribution_bit",
        }
    }
}

#[cfg(all(test, not(feature = "shuttle")))]
pub(crate) mod tests {
    use super::super::tests::{BD, H};
    use super::sort_by_aggregation_bit_and_breakdown_key;
    use crate::ff::{Field, Fp31};
    use crate::protocol::attribution::accumulate_credit::tests::AttributionTestInput;
    use crate::protocol::attribution::CappedCreditsWithAggregationBit;
    use crate::protocol::QueryId;
    use crate::rand::Rng;
    use crate::secret_sharing::Replicated;
    use crate::test_fixture::{IntoShares, Reconstruct, Runner, TestWorld};
    use rand::{distributions::Standard, prelude::Distribution};

    impl<F> IntoShares<CappedCreditsWithAggregationBit<F>> for AttributionTestInput<F>
    where
        F: Field + IntoShares<Replicated<F>>,
        Standard: Distribution<F>,
    {
        fn share_with<R: Rng>(self, rng: &mut R) -> [CappedCreditsWithAggregationBit<F>; 3] {
            let [a0, a1, a2] = self.0[0].share_with(rng);
            let [b0, b1, b2] = self.0[1].share_with(rng);
            let [c0, c1, c2] = self.0[2].share_with(rng);
            let [d0, d1, d2] = self.0[3].share_with(rng);
            [
                CappedCreditsWithAggregationBit {
                    helper_bit: a0,
                    breakdown_key: b0,
                    credit: c0,
                    aggregation_bit: d0,
                },
                CappedCreditsWithAggregationBit {
                    helper_bit: a1,
                    breakdown_key: b1,
                    credit: c1,
                    aggregation_bit: d1,
                },
                CappedCreditsWithAggregationBit {
                    helper_bit: a2,
                    breakdown_key: b2,
                    credit: c2,
                    aggregation_bit: d2,
                },
            ]
        }
    }

    impl<F: Field> Reconstruct<AttributionTestInput<F>> for [CappedCreditsWithAggregationBit<F>; 3] {
        fn reconstruct(&self) -> AttributionTestInput<F> {
            [&self[0], &self[1], &self[2]].reconstruct()
        }
    }

    impl<F: Field> Reconstruct<AttributionTestInput<F>> for [&CappedCreditsWithAggregationBit<F>; 3] {
        fn reconstruct(&self) -> AttributionTestInput<F> {
            let s0 = &self[0];
            let s1 = &self[1];
            let s2 = &self[2];

            let helper_bit = (&s0.helper_bit, &s1.helper_bit, &s2.helper_bit).reconstruct();

            let breakdown_key =
                (&s0.breakdown_key, &s1.breakdown_key, &s2.breakdown_key).reconstruct();
            let credit = (&s0.credit, &s1.credit, &s2.credit).reconstruct();

            let aggregation_bit = (
                &s0.aggregation_bit,
                &s1.aggregation_bit,
                &s2.aggregation_bit,
            )
                .reconstruct();

            AttributionTestInput([helper_bit, breakdown_key, credit, aggregation_bit])
        }
    }

    #[tokio::test]
    pub async fn sort() {
        // Result from CreditCapping, plus AggregateCredit pre-processing
        const RAW_INPUT: &[[u128; 4]; 27] = &[
            // helper_bit, breakdown_key, credit, aggregation_bit

            // AggregateCredit protocol initializes helper_bits with 1 for all input rows.
            [H[1], BD[3], 0, 1],
            [H[1], BD[4], 0, 1],
            [H[1], BD[4], 18, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[1], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[2], 2, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[2], 0, 1],
            [H[1], BD[2], 10, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[5], 6, 1],
            [H[1], BD[0], 0, 1],
            // AggregateCredit protocol appends unique breakdown_keys with all
            // other fields with 0.
            [H[0], BD[0], 0, 0],
            [H[0], BD[1], 0, 0],
            [H[0], BD[2], 0, 0],
            [H[0], BD[3], 0, 0],
            [H[0], BD[4], 0, 0],
            [H[0], BD[5], 0, 0],
            [H[0], BD[6], 0, 0],
            [H[0], BD[7], 0, 0],
        ];

        // sorted by aggregation_bit, then by breakdown_key
        const EXPECTED: &[[u128; 4]; 27] = &[
            // breakdown_key 0
            [H[0], BD[0], 0, 0],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            [H[1], BD[0], 0, 1],
            // breakdown_key 1
            [H[0], BD[1], 0, 0],
            [H[1], BD[1], 0, 1],
            // breakdown_key 2
            [H[0], BD[2], 0, 0],
            [H[1], BD[2], 2, 1],
            [H[1], BD[2], 0, 1],
            [H[1], BD[2], 10, 1],
            // breakdown_key 3
            [H[0], BD[3], 0, 0],
            [H[1], BD[3], 0, 1],
            // breakdown_key 4
            [H[0], BD[4], 0, 0],
            [H[1], BD[4], 0, 1],
            [H[1], BD[4], 18, 1],
            // breakdown_key 5
            [H[0], BD[5], 0, 0],
            [H[1], BD[5], 6, 1],
            // breakdown_key 6
            [H[0], BD[6], 0, 0],
            // breakdown_key 7
            [H[0], BD[7], 0, 0],
        ];

        let input = RAW_INPUT.map(|x| {
            AttributionTestInput([
                Fp31::from(x[0]),
                Fp31::from(x[1]),
                Fp31::from(x[2]),
                Fp31::from(x[3]),
            ])
        });

        let world = TestWorld::new(QueryId);
        let result = world
            .semi_honest(input, |ctx, share| async move {
                sort_by_aggregation_bit_and_breakdown_key(ctx, &share)
                    .await
                    .unwrap()
            })
            .await
            .reconstruct();

        assert_eq!(RAW_INPUT.len(), result.len());

        for (i, expected) in EXPECTED.iter().enumerate() {
            assert_eq!(*expected, result[i].0.map(|x| x.as_u128()));
        }
    }
}
