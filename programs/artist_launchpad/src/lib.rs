//! MILK: AI artists launchpad, on Meteora's Dynamic Bonding Curve.
//!
//! Every artist and every song is a token launched by its creator on a Meteora DBC bonding curve, priced in SOL, so it
//! trades everywhere DBC tokens trade (Jupiter, the trading terminals and bots) from its first second, and graduates to
//! a Meteora DAMM v2 pool by itself when the curve sells out. This program is what sits around those pools:
//!
//! - Registry: an artist (or a song of an artist) exists for the platform once registered here, co-signed by the
//!   operator (only artists and songs the studio generated) and by the artist's creator (only they release its songs).
//!   Registration checks on-chain that the DBC pool is ours: one of the platform's two DBC configs, this mint, this
//!   creator.
//! - Fees: the DBC configs give the creator 40% of the trading fees (paid by DBC to the creator) and the rest to the
//!   partner, a PDA of this program (`fees`). Anyone can push the partner share out (`claim_fees`): half to the platform,
//!   half to the artist's treasury, which makes it 30 / 40 / 30 of what Meteora's protocol share leaves. The same split
//!   applies to the fees of the program's locked position once graduated (`claim_pool_fees`). The pool creation fee
//!   every creator pays at launch goes to the platform (`claim_creation_fee`).
//! - Treasury: each artist has a treasury (PDA, wrapped SOL) that pays its generations (music, visuals). The operator
//!   spends it, within a daily cap, to the platform's ops wallet only; the admin can freeze all treasury spending.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_interface::{transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("Df5CVncF1g3erUdfa3aGJUUxHELXqnDK3kHzZa33Rrmr");

// Meteora Dynamic Bonding Curve: where the tokens are launched and traded until graduation.
declare_program!(dynamic_bonding_curve);
// Meteora DAMM v2 (cp-amm): where they trade after graduation (DBC migrates them itself).
declare_program!(damm_v2);

pub const CONFIG_SEED: &[u8] = b"config";
pub const ARTIST_SEED: &[u8] = b"artist";
pub const SONG_SEED: &[u8] = b"song";
pub const TREASURY_SEED: &[u8] = b"treasury";
/// The DBC partner (fee claimer and leftover receiver of the platform's DBC configs).
pub const FEES_SEED: &[u8] = b"fees";

pub const BPS: u64 = 10_000;
pub const DAY: i64 = 86_400;

/// Only this key can create the config, so nobody can initialize the launchpad before its owner.
#[cfg(not(feature = "test-initializer"))]
pub const INITIALIZER: Pubkey = pubkey!("ECDVeDxvwxuw5qpqp6W8LheWHT7BCiZ2p1ujwpvqGMEb");
/// Tests: the key derived from the seed [9; 32].
#[cfg(feature = "test-initializer")]
pub const INITIALIZER: Pubkey = pubkey!("J2xccRtuG43drESLYznHhLhQkLTdfepcKYbiQ9BsJVaf");

/// Wrapped SOL: the quote of every pool.
pub const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

pub const SOURCE_CURVE: u8 = 0;
pub const SOURCE_POOL: u8 = 1;

#[program]
pub mod artist_launchpad {
    use super::*;

    pub fn initialize_config(ctx: Context<InitializeConfig>, params: ConfigParams) -> Result<()> {
        require_keys_eq!(ctx.accounts.admin.key(), INITIALIZER, LaunchpadError::Unauthorized);
        params.validate()?;
        let config = &mut ctx.accounts.config;
        config.admin = ctx.accounts.admin.key();
        config.bump = ctx.bumps.config;
        config.fees_bump = ctx.bumps.fees;
        config.apply(&params);
        Ok(())
    }

    /// New DBC configs apply to launches from now on; pools keep the config they were created with.
    pub fn update_config(ctx: Context<AdminOnly>, params: ConfigParams) -> Result<()> {
        params.validate()?;
        ctx.accounts.config.apply(&params);
        Ok(())
    }

    pub fn set_admin(ctx: Context<AdminOnly>, new_admin: Pubkey) -> Result<()> {
        ctx.accounts.config.admin = new_admin;
        Ok(())
    }

    /// Kill switch: freezes every treasury's spending (trading and fee claims keep working).
    pub fn set_treasury_frozen(ctx: Context<AdminOnly>, frozen: bool) -> Result<()> {
        ctx.accounts.config.treasury_frozen = frozen;
        Ok(())
    }

    /// Registers an artist whose token the creator just launched on the platform's artist DBC config (same
    /// transaction). Co-signed by the operator; the creator pays the rent.
    pub fn register_artist(ctx: Context<RegisterArtist>) -> Result<()> {
        let pool = read_virtual_pool(&ctx.accounts.dbc_pool)?;
        require_keys_eq!(pool.config, ctx.accounts.config.artist_dbc_config, LaunchpadError::WrongPool);
        require_keys_eq!(pool.base_mint, ctx.accounts.mint.key(), LaunchpadError::WrongPool);
        require_keys_eq!(pool.creator, ctx.accounts.creator.key(), LaunchpadError::WrongPool);
        let artist = &mut ctx.accounts.artist;
        artist.creator = ctx.accounts.creator.key();
        artist.mint = ctx.accounts.mint.key();
        artist.pool = ctx.accounts.dbc_pool.key();
        artist.song_count = 0;
        artist.spend_day = 0;
        artist.spent_today = 0;
        artist.bump = ctx.bumps.artist;
        artist.treasury_bump = ctx.bumps.treasury;
        emit!(ArtistRegistered { artist: artist.key(), mint: artist.mint, pool: artist.pool, creator: artist.creator });
        Ok(())
    }

    /// Registers a song the artist's creator just launched on the platform's song DBC config (same transaction): its
    /// index under the artist, and the artist whose treasury its fees feed. Creator + operator.
    pub fn register_song(ctx: Context<RegisterSong>) -> Result<()> {
        let pool = read_virtual_pool(&ctx.accounts.dbc_pool)?;
        require_keys_eq!(pool.config, ctx.accounts.config.song_dbc_config, LaunchpadError::WrongPool);
        require_keys_eq!(pool.base_mint, ctx.accounts.mint.key(), LaunchpadError::WrongPool);
        require_keys_eq!(pool.creator, ctx.accounts.creator.key(), LaunchpadError::WrongPool);
        let artist = &mut ctx.accounts.artist;
        let index = artist.song_count;
        artist.song_count = index.checked_add(1).ok_or(LaunchpadError::MathOverflow)?;
        let song = &mut ctx.accounts.song;
        song.artist = artist.mint;
        song.mint = ctx.accounts.mint.key();
        song.pool = ctx.accounts.dbc_pool.key();
        song.index = index;
        song.bump = ctx.bumps.song;
        emit!(SongRegistered { artist_mint: artist.mint, mint: song.mint, pool: song.pool, index });
        Ok(())
    }

    /// Pushes out the partner's share of a curve's trading fees: half to the platform, half to the artist's treasury.
    /// Permissionless.
    pub fn claim_fees(ctx: Context<ClaimFees>) -> Result<()> {
        let mint = ctx.accounts.base_mint.key();
        check_owner_of_mint(&ctx.accounts.artist, ctx.accounts.song.as_deref(), mint)?;
        let pool = read_virtual_pool(&ctx.accounts.dbc_pool)?;
        let ours = [ctx.accounts.config.artist_dbc_config, ctx.accounts.config.song_dbc_config];
        require!(ours.contains(&pool.config) && pool.base_mint == mint, LaunchpadError::WrongPool);

        let before = ctx.accounts.fees_quote.amount;
        let fees_seeds: &[&[&[u8]]] = &[&[FEES_SEED, &[ctx.accounts.config.fees_bump]]];
        let a = &ctx.accounts;
        dynamic_bonding_curve::cpi::claim_trading_fee(
            CpiContext::new_with_signer(
                a.dbc_program.key(),
                dynamic_bonding_curve::cpi::accounts::ClaimTradingFee {
                    pool_authority: a.dbc_pool_authority.to_account_info(),
                    config: a.dbc_config.to_account_info(),
                    pool: a.dbc_pool.to_account_info(),
                    token_a_account: a.fees_base.to_account_info(),
                    token_b_account: a.fees_quote.to_account_info(),
                    base_vault: a.base_vault.to_account_info(),
                    quote_vault: a.quote_vault.to_account_info(),
                    base_mint: a.base_mint.to_account_info(),
                    quote_mint: a.quote_mint.to_account_info(),
                    fee_claimer: a.fees.to_account_info(),
                    token_base_program: a.base_token_program.to_account_info(),
                    token_quote_program: a.quote_token_program.to_account_info(),
                    event_authority: a.dbc_event_authority.to_account_info(),
                    program: a.dbc_program.to_account_info(),
                },
                fees_seeds,
            ),
            u64::MAX,
            u64::MAX,
        )?;
        let mut fees_quote = ctx.accounts.fees_quote.clone();
        fees_quote.reload()?;
        let claimed = fees_quote.amount.saturating_sub(before);
        split(
            &ctx.accounts.config,
            &ctx.accounts.fees,
            &fees_quote,
            &ctx.accounts.quote_mint,
            &ctx.accounts.platform_quote,
            &ctx.accounts.treasury_quote,
            &ctx.accounts.quote_token_program,
            fees_seeds,
            claimed,
            mint,
            SOURCE_CURVE,
        )
    }

    /// Pushes out the pool creation fee the creator paid at launch, to the platform. Permissionless.
    pub fn claim_creation_fee(ctx: Context<ClaimCreationFee>) -> Result<()> {
        let pool = read_virtual_pool(&ctx.accounts.dbc_pool)?;
        let ours = [ctx.accounts.config.artist_dbc_config, ctx.accounts.config.song_dbc_config];
        require!(ours.contains(&pool.config), LaunchpadError::WrongPool);
        let fees_seeds: &[&[&[u8]]] = &[&[FEES_SEED, &[ctx.accounts.config.fees_bump]]];
        let a = &ctx.accounts;
        dynamic_bonding_curve::cpi::claim_partner_pool_creation_fee(CpiContext::new_with_signer(
            a.dbc_program.key(),
            dynamic_bonding_curve::cpi::accounts::ClaimPartnerPoolCreationFee {
                config: a.dbc_config.to_account_info(),
                pool: a.dbc_pool.to_account_info(),
                fee_claimer: a.fees.to_account_info(),
                fee_receiver: a.platform_wallet.to_account_info(),
                event_authority: a.dbc_event_authority.to_account_info(),
                program: a.dbc_program.to_account_info(),
            },
            fees_seeds,
        ))
    }

    /// After graduation: pushes out the fees of the partner's locked DAMM v2 position (DBC gives it to the fee
    /// claimer at migration), with the same split. Permissionless.
    pub fn claim_pool_fees(ctx: Context<ClaimPoolFees>) -> Result<()> {
        let mint = ctx.accounts.base_mint.key();
        check_owner_of_mint(&ctx.accounts.artist, ctx.accounts.song.as_deref(), mint)?;
        let pool = read_damm_pool(&ctx.accounts.pool)?;
        require!(pool.token_a_mint == mint && pool.token_b_mint == WSOL, LaunchpadError::WrongPool);

        let before = ctx.accounts.fees_quote.amount;
        let fees_seeds: &[&[&[u8]]] = &[&[FEES_SEED, &[ctx.accounts.config.fees_bump]]];
        let a = &ctx.accounts;
        // DAMM v2 checks that the position's NFT sits in an account owned by the signer: only our position.
        damm_v2::cpi::claim_position_fee(CpiContext::new_with_signer(
            a.damm_program.key(),
            damm_v2::cpi::accounts::ClaimPositionFee {
                pool_authority: a.pool_authority.to_account_info(),
                pool: a.pool.to_account_info(),
                position: a.position.to_account_info(),
                token_a_account: a.fees_base.to_account_info(),
                token_b_account: a.fees_quote.to_account_info(),
                token_a_vault: a.pool_base_vault.to_account_info(),
                token_b_vault: a.pool_quote_vault.to_account_info(),
                token_a_mint: a.base_mint.to_account_info(),
                token_b_mint: a.quote_mint.to_account_info(),
                position_nft_account: a.position_nft_account.to_account_info(),
                signer: a.fees.to_account_info(),
                token_a_program: a.base_token_program.to_account_info(),
                token_b_program: a.quote_token_program.to_account_info(),
                event_authority: a.damm_event_authority.to_account_info(),
                program: a.damm_program.to_account_info(),
            },
            fees_seeds,
        ))?;
        let mut fees_quote = ctx.accounts.fees_quote.clone();
        fees_quote.reload()?;
        let claimed = fees_quote.amount.saturating_sub(before);
        split(
            &ctx.accounts.config,
            &ctx.accounts.fees,
            &fees_quote,
            &ctx.accounts.quote_mint,
            &ctx.accounts.platform_quote,
            &ctx.accounts.treasury_quote,
            &ctx.accounts.quote_token_program,
            fees_seeds,
            claimed,
            mint,
            SOURCE_POOL,
        )
    }

    /// The operator pays an artist's generations (music, visuals) from its treasury, to the platform's ops wallet,
    /// within a daily cap: `treasury_daily_cap_bps` of what the treasury held at the start of the day.
    pub fn treasury_spend(ctx: Context<TreasurySpend>, amount: u64, memo: String) -> Result<()> {
        let config = &ctx.accounts.config;
        require!(!config.treasury_frozen, LaunchpadError::TreasuryFrozen);
        require!(amount > 0, LaunchpadError::ZeroAmount);
        require!(memo.len() <= 64, LaunchpadError::InvalidMetadata);

        let now = Clock::get()?.unix_timestamp;
        let today = now / DAY;
        let artist = &mut ctx.accounts.artist;
        if artist.spend_day != today {
            artist.spend_day = today;
            artist.spent_today = 0;
        }
        let balance = ctx.accounts.treasury_quote.amount;
        let base = balance.checked_add(artist.spent_today).ok_or(LaunchpadError::MathOverflow)?;
        let cap = (base as u128 * config.treasury_daily_cap_bps as u128 / BPS as u128) as u64;
        let spent = artist.spent_today.checked_add(amount).ok_or(LaunchpadError::MathOverflow)?;
        require!(spent <= cap, LaunchpadError::DailyCapExceeded);
        artist.spent_today = spent;

        let mint = artist.mint;
        let seeds: &[&[&[u8]]] = &[&[TREASURY_SEED, mint.as_ref(), &[artist.treasury_bump]]];
        transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.quote_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.treasury_quote.to_account_info(),
                    mint: ctx.accounts.quote_mint.to_account_info(),
                    to: ctx.accounts.ops_quote.to_account_info(),
                    authority: ctx.accounts.treasury.to_account_info(),
                },
                seeds,
            ),
            amount,
            ctx.accounts.quote_mint.decimals,
        )?;
        emit!(TreasurySpent { artist_mint: mint, quote_mint: ctx.accounts.quote_mint.key(), amount, memo });
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------------ helpers

/// The fees of `mint` belong to this artist: its own token, or one of its registered songs.
fn check_owner_of_mint(artist: &Account<Artist>, song: Option<&Account<Song>>, mint: Pubkey) -> Result<()> {
    match song {
        Some(song) => require!(song.artist == artist.mint && song.mint == mint, LaunchpadError::WrongPool),
        None => require_keys_eq!(artist.mint, mint, LaunchpadError::WrongPool),
    }
    Ok(())
}

/// Half (`platform_share_bps`) of the claimed partner fees to the platform, the rest to the artist's treasury.
#[allow(clippy::too_many_arguments)]
fn split<'info>(
    config: &Account<'info, Config>,
    fees: &SystemAccount<'info>,
    from: &InterfaceAccount<'info, TokenAccount>,
    quote_mint: &InterfaceAccount<'info, Mint>,
    platform_quote: &InterfaceAccount<'info, TokenAccount>,
    treasury_quote: &InterfaceAccount<'info, TokenAccount>,
    token_program: &Interface<'info, TokenInterface>,
    seeds: &[&[&[u8]]],
    claimed: u64,
    mint: Pubkey,
    source: u8,
) -> Result<()> {
    let platform = (claimed as u128 * config.platform_share_bps as u128 / BPS as u128) as u64;
    let treasury = claimed - platform;
    for (amount, to) in [(platform, platform_quote), (treasury, treasury_quote)] {
        if amount == 0 {
            continue;
        }
        transfer_checked(
            CpiContext::new_with_signer(
                token_program.key(),
                TransferChecked { from: from.to_account_info(), mint: quote_mint.to_account_info(), to: to.to_account_info(), authority: fees.to_account_info() },
                seeds,
            ),
            amount,
            quote_mint.decimals,
        )?;
    }
    emit!(FeesClaimed { mint, platform, treasury, source });
    Ok(())
}

/// A DBC virtual pool's state (the account must belong to the DBC program).
fn read_virtual_pool(info: &AccountInfo) -> Result<dynamic_bonding_curve::types::PoolState> {
    require_keys_eq!(*info.owner, dynamic_bonding_curve::ID, LaunchpadError::WrongPool);
    let data = info.try_borrow_data()?;
    require!(data.len() >= 8 && data[..8] == *dynamic_bonding_curve::accounts::VirtualPool::DISCRIMINATOR, LaunchpadError::WrongPool);
    let size = core::mem::size_of::<dynamic_bonding_curve::types::PoolState>();
    require!(data.len() >= 8 + size, LaunchpadError::WrongPool);
    Ok(bytemuck::pod_read_unaligned(&data[8..8 + size]))
}

/// A DAMM v2 pool (the account must belong to DAMM v2).
fn read_damm_pool(info: &AccountInfo) -> Result<damm_v2::accounts::Pool> {
    require_keys_eq!(*info.owner, damm_v2::ID, LaunchpadError::WrongPool);
    let data = info.try_borrow_data()?;
    require!(data.len() >= 8 && data[..8] == *damm_v2::accounts::Pool::DISCRIMINATOR, LaunchpadError::WrongPool);
    let size = core::mem::size_of::<damm_v2::accounts::Pool>();
    require!(data.len() >= 8 + size, LaunchpadError::WrongPool);
    Ok(bytemuck::pod_read_unaligned(&data[8..8 + size]))
}

// ------------------------------------------------------------------------------------------------ state

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug)]
pub struct ConfigParams {
    pub operator: Pubkey,
    pub platform_wallet: Pubkey,
    pub ops_wallet: Pubkey,
    /// Platform's part of the partner fees (the rest goes to the artist's treasury). 5000 = half.
    pub platform_share_bps: u16,
    pub treasury_daily_cap_bps: u16,
    /// The platform's DBC configs (fee claimer: the `fees` PDA), artists and songs, both in SOL.
    pub artist_dbc_config: Pubkey,
    pub song_dbc_config: Pubkey,
}

impl ConfigParams {
    fn validate(&self) -> Result<()> {
        require!(self.platform_share_bps as u64 <= BPS, LaunchpadError::InvalidConfig);
        require!(self.treasury_daily_cap_bps as u64 <= BPS, LaunchpadError::InvalidConfig);
        require!(self.artist_dbc_config != self.song_dbc_config, LaunchpadError::InvalidConfig);
        Ok(())
    }
}

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    /// Backend key: co-signs registrations, spends treasuries for generations.
    pub operator: Pubkey,
    /// Owner of the platform's fee accounts.
    pub platform_wallet: Pubkey,
    /// Receives treasury spending (pays the generation providers).
    pub ops_wallet: Pubkey,
    pub platform_share_bps: u16,
    pub treasury_daily_cap_bps: u16,
    pub treasury_frozen: bool,
    pub artist_dbc_config: Pubkey,
    pub song_dbc_config: Pubkey,
    pub bump: u8,
    pub fees_bump: u8,
}

impl Config {
    fn apply(&mut self, p: &ConfigParams) {
        self.operator = p.operator;
        self.platform_wallet = p.platform_wallet;
        self.ops_wallet = p.ops_wallet;
        self.platform_share_bps = p.platform_share_bps;
        self.treasury_daily_cap_bps = p.treasury_daily_cap_bps;
        self.artist_dbc_config = p.artist_dbc_config;
        self.song_dbc_config = p.song_dbc_config;
    }
}

#[account]
#[derive(InitSpace)]
pub struct Artist {
    pub creator: Pubkey,
    pub mint: Pubkey,
    /// Its DBC virtual pool.
    pub pool: Pubkey,
    pub song_count: u32,
    pub spend_day: i64,
    pub spent_today: u64,
    pub bump: u8,
    pub treasury_bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Song {
    /// The artist's mint.
    pub artist: Pubkey,
    pub mint: Pubkey,
    pub pool: Pubkey,
    pub index: u32,
    pub bump: u8,
}

// ------------------------------------------------------------------------------------------------ accounts

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + Config::INIT_SPACE, seeds = [CONFIG_SEED], bump)]
    pub config: Account<'info, Config>,
    /// The DBC partner PDA (no data).
    #[account(seeds = [FEES_SEED], bump)]
    pub fees: SystemAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ LaunchpadError::Unauthorized)]
    pub config: Account<'info, Config>,
}

#[derive(Accounts)]
pub struct RegisterArtist<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(address = config.operator @ LaunchpadError::Unauthorized)]
    pub operator: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(init, payer = creator, space = 8 + Artist::INIT_SPACE, seeds = [ARTIST_SEED, mint.key().as_ref()], bump)]
    pub artist: Box<Account<'info, Artist>>,
    /// CHECK: the artist's treasury PDA (only its bump is kept).
    #[account(seeds = [TREASURY_SEED, mint.key().as_ref()], bump)]
    pub treasury: UncheckedAccount<'info>,
    /// CHECK: the artist token, checked against the pool.
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the DBC virtual pool (owner, discriminator, config, mint and creator checked).
    pub dbc_pool: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RegisterSong<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(address = config.operator @ LaunchpadError::Unauthorized)]
    pub operator: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [ARTIST_SEED, artist.mint.as_ref()], bump = artist.bump, has_one = creator @ LaunchpadError::NotArtistCreator)]
    pub artist: Box<Account<'info, Artist>>,
    #[account(init, payer = creator, space = 8 + Song::INIT_SPACE, seeds = [SONG_SEED, artist.mint.as_ref(), &artist.song_count.to_le_bytes()], bump)]
    pub song: Box<Account<'info, Song>>,
    /// CHECK: the song token, checked against the pool.
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the DBC virtual pool (owner, discriminator, config, mint and creator checked).
    pub dbc_pool: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClaimFees<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [FEES_SEED], bump = config.fees_bump)]
    pub fees: SystemAccount<'info>,
    #[account(seeds = [ARTIST_SEED, artist.mint.as_ref()], bump = artist.bump)]
    pub artist: Box<Account<'info, Artist>>,
    /// The song, when the pool is a song's.
    #[account(seeds = [SONG_SEED, song.artist.as_ref(), &song.index.to_le_bytes()], bump = song.bump)]
    pub song: Option<Box<Account<'info, Song>>>,
    /// CHECK: the artist's treasury PDA.
    #[account(seeds = [TREASURY_SEED, artist.mint.as_ref()], bump = artist.treasury_bump)]
    pub treasury: UncheckedAccount<'info>,
    /// CHECK: the DBC virtual pool (checked: ours, this mint).
    #[account(mut)]
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: the pool's DBC config (checked by DBC).
    pub dbc_config: UncheckedAccount<'info>,
    /// CHECK: DBC pool authority (checked by DBC).
    pub dbc_pool_authority: UncheckedAccount<'info>,
    #[account(mint::token_program = base_token_program)]
    pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address = WSOL, mint::token_program = quote_token_program)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: checked by DBC.
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: checked by DBC.
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = payer, associated_token::mint = base_mint, associated_token::authority = fees, associated_token::token_program = base_token_program)]
    pub fees_base: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init_if_needed, payer = payer, associated_token::mint = quote_mint, associated_token::authority = fees, associated_token::token_program = quote_token_program)]
    pub fees_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: checked against the config.
    #[account(address = config.platform_wallet @ LaunchpadError::Unauthorized)]
    pub platform_wallet: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = payer, associated_token::mint = quote_mint, associated_token::authority = platform_wallet, associated_token::token_program = quote_token_program)]
    pub platform_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init_if_needed, payer = payer, associated_token::mint = quote_mint, associated_token::authority = treasury, associated_token::token_program = quote_token_program)]
    pub treasury_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    pub base_token_program: Interface<'info, TokenInterface>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    /// CHECK: DBC event authority (checked by DBC).
    pub dbc_event_authority: UncheckedAccount<'info>,
    pub dbc_program: Program<'info, dynamic_bonding_curve::program::DynamicBondingCurve>,
}

#[derive(Accounts)]
pub struct ClaimCreationFee<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [FEES_SEED], bump = config.fees_bump)]
    pub fees: SystemAccount<'info>,
    /// CHECK: the DBC virtual pool (checked: ours).
    #[account(mut)]
    pub dbc_pool: UncheckedAccount<'info>,
    /// CHECK: the pool's DBC config (checked by DBC).
    pub dbc_config: UncheckedAccount<'info>,
    /// CHECK: checked against the config.
    #[account(mut, address = config.platform_wallet @ LaunchpadError::Unauthorized)]
    pub platform_wallet: UncheckedAccount<'info>,
    /// CHECK: DBC event authority (checked by DBC).
    pub dbc_event_authority: UncheckedAccount<'info>,
    pub dbc_program: Program<'info, dynamic_bonding_curve::program::DynamicBondingCurve>,
}

#[derive(Accounts)]
pub struct ClaimPoolFees<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [FEES_SEED], bump = config.fees_bump)]
    pub fees: SystemAccount<'info>,
    #[account(seeds = [ARTIST_SEED, artist.mint.as_ref()], bump = artist.bump)]
    pub artist: Box<Account<'info, Artist>>,
    #[account(seeds = [SONG_SEED, song.artist.as_ref(), &song.index.to_le_bytes()], bump = song.bump)]
    pub song: Option<Box<Account<'info, Song>>>,
    /// CHECK: the artist's treasury PDA.
    #[account(seeds = [TREASURY_SEED, artist.mint.as_ref()], bump = artist.treasury_bump)]
    pub treasury: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 pool authority (checked by DAMM v2).
    pub pool_authority: UncheckedAccount<'info>,
    /// CHECK: the graduated DAMM v2 pool (checked: this mint against SOL).
    pub pool: UncheckedAccount<'info>,
    /// CHECK: the partner's position (checked by DAMM v2 against its NFT, which the signer must hold).
    #[account(mut)]
    pub position: UncheckedAccount<'info>,
    /// CHECK: checked by DAMM v2.
    pub position_nft_account: UncheckedAccount<'info>,
    #[account(mint::token_program = base_token_program)]
    pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address = WSOL, mint::token_program = quote_token_program)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: checked by DAMM v2.
    #[account(mut)]
    pub pool_base_vault: UncheckedAccount<'info>,
    /// CHECK: checked by DAMM v2.
    #[account(mut)]
    pub pool_quote_vault: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = payer, associated_token::mint = base_mint, associated_token::authority = fees, associated_token::token_program = base_token_program)]
    pub fees_base: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init_if_needed, payer = payer, associated_token::mint = quote_mint, associated_token::authority = fees, associated_token::token_program = quote_token_program)]
    pub fees_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: checked against the config.
    #[account(address = config.platform_wallet @ LaunchpadError::Unauthorized)]
    pub platform_wallet: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = payer, associated_token::mint = quote_mint, associated_token::authority = platform_wallet, associated_token::token_program = quote_token_program)]
    pub platform_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init_if_needed, payer = payer, associated_token::mint = quote_mint, associated_token::authority = treasury, associated_token::token_program = quote_token_program)]
    pub treasury_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    pub base_token_program: Interface<'info, TokenInterface>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    /// CHECK: DAMM v2 event authority (checked by DAMM v2).
    pub damm_event_authority: UncheckedAccount<'info>,
    pub damm_program: Program<'info, damm_v2::program::CpAmm>,
}

#[derive(Accounts)]
pub struct TreasurySpend<'info> {
    #[account(address = config.operator @ LaunchpadError::Unauthorized)]
    pub operator: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [ARTIST_SEED, artist.mint.as_ref()], bump = artist.bump)]
    pub artist: Box<Account<'info, Artist>>,
    /// CHECK: the artist's treasury PDA.
    #[account(seeds = [TREASURY_SEED, artist.mint.as_ref()], bump = artist.treasury_bump)]
    pub treasury: UncheckedAccount<'info>,
    #[account(mint::token_program = quote_token_program)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, associated_token::mint = quote_mint, associated_token::authority = treasury, associated_token::token_program = quote_token_program)]
    pub treasury_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = quote_mint, token::authority = config.ops_wallet, token::token_program = quote_token_program)]
    pub ops_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    pub quote_token_program: Interface<'info, TokenInterface>,
}

// ------------------------------------------------------------------------------------------------ events & errors

#[event]
pub struct ArtistRegistered {
    pub artist: Pubkey,
    pub mint: Pubkey,
    pub pool: Pubkey,
    pub creator: Pubkey,
}

#[event]
pub struct SongRegistered {
    pub artist_mint: Pubkey,
    pub mint: Pubkey,
    pub pool: Pubkey,
    pub index: u32,
}

/// Partner fees pushed out: SOURCE_CURVE (DBC) or SOURCE_POOL (the graduated DAMM v2 position).
#[event]
pub struct FeesClaimed {
    pub mint: Pubkey,
    pub platform: u64,
    pub treasury: u64,
    pub source: u8,
}

#[event]
pub struct TreasurySpent {
    pub artist_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub amount: u64,
    pub memo: String,
}

#[error_code]
pub enum LaunchpadError {
    #[msg("Not allowed")]
    Unauthorized,
    #[msg("Invalid configuration")]
    InvalidConfig,
    #[msg("Invalid memo")]
    InvalidMetadata,
    #[msg("Only the artist's creator can do this")]
    NotArtistCreator,
    #[msg("Amount must be positive")]
    ZeroAmount,
    #[msg("Treasury spending is frozen")]
    TreasuryFrozen,
    #[msg("Daily treasury spending cap reached")]
    DailyCapExceeded,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Not this token's pool")]
    WrongPool,
}
