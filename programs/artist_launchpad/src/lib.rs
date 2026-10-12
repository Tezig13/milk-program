//! MILK: AI artists launchpad, on pump.fun.
//!
//! Every artist is a pump.fun coin quoted in SOL; every song is a pump.fun coin quoted in its artist's coin (a
//! "depth 1" coin, pump.fun's custom pairs), so a song is bought with the artist. pump.fun runs the curves, the
//! graduation to PumpSwap and the trading; bots and terminals see ordinary pump.fun coins. This program is what sits
//! around them:
//!
//! - Registry: an artist (or a song of an artist) exists for the platform once registered here, co-signed by the
//!   operator (only artists and songs the studio generated) and by the artist's creator (only they release its songs).
//!   Registration checks the pump.fun bonding curve on-chain: its creator is this token's `vault` (a PDA of this
//!   program), and for a song, its quote is the artist's coin with the platform's creator fee rate. The creator pays the
//!   launch fee to the platform there.
//! - Fees: pump.fun pays a coin's creator fees to its creator, here the token's `vault`, which nobody can sign for
//!   but this program. Collecting them out of pump.fun (curve and PumpSwap) is permissionless; `distribute_artist` /
//!   `distribute_song` then split what the vault holds: 30% platform, 40% the artist's creator, 30% the artist's
//!   treasury (artists, in SOL) or burnt (songs, in the artist's coin).
//! - Treasury: each artist has a treasury (PDA, wrapped SOL) that pays its generations (music, visuals). The operator
//!   spends it, within a daily cap, to the platform's ops wallet only; the admin can freeze all treasury spending.

use anchor_lang::prelude::*;
use anchor_lang::system_program;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_interface::{
    burn, close_account, sync_native, transfer_checked, Burn, CloseAccount, Mint, SyncNative, TokenAccount, TokenInterface, TransferChecked,
};

declare_id!("Df5CVncF1g3erUdfa3aGJUUxHELXqnDK3kHzZa33Rrmr");

pub const CONFIG_SEED: &[u8] = b"config";
pub const ARTIST_SEED: &[u8] = b"artist";
pub const SONG_SEED: &[u8] = b"song";
pub const TREASURY_SEED: &[u8] = b"treasury";
/// A token's pump.fun creator: receives its creator fees, signed for only by this program.
pub const VAULT_SEED: &[u8] = b"vault";

pub const BPS: u64 = 10_000;
pub const DAY: i64 = 86_400;

/// Only this key can create the config, so nobody can initialize the launchpad before its owner.
#[cfg(not(feature = "test-initializer"))]
pub const INITIALIZER: Pubkey = pubkey!("ECDVeDxvwxuw5qpqp6W8LheWHT7BCiZ2p1ujwpvqGMEb");
/// Tests: the key derived from the seed [9; 32].
#[cfg(feature = "test-initializer")]
pub const INITIALIZER: Pubkey = pubkey!("J2xccRtuG43drESLYznHhLhQkLTdfepcKYbiQ9BsJVaf");

/// Wrapped SOL: the treasuries' currency.
pub const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
/// pump.fun's bonding curve program.
pub const PUMP: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
/// `BondingCurve` account discriminator (sha256("account:BondingCurve")[..8]).
pub const BONDING_CURVE_DISCRIMINATOR: [u8; 8] = [23, 183, 248, 55, 96, 216, 172, 96];

#[program]
pub mod artist_launchpad {
    use super::*;

    pub fn initialize_config(ctx: Context<InitializeConfig>, params: ConfigParams) -> Result<()> {
        require_keys_eq!(ctx.accounts.admin.key(), INITIALIZER, LaunchpadError::Unauthorized);
        params.validate()?;
        let config = &mut ctx.accounts.config;
        config.admin = ctx.accounts.admin.key();
        config.bump = ctx.bumps.config;
        config.apply(&params);
        Ok(())
    }

    /// New terms apply to launches and distributions from now on.
    pub fn update_config(ctx: Context<AdminOnly>, params: ConfigParams) -> Result<()> {
        params.validate()?;
        ctx.accounts.config.apply(&params);
        Ok(())
    }

    pub fn set_admin(ctx: Context<AdminOnly>, new_admin: Pubkey) -> Result<()> {
        ctx.accounts.config.admin = new_admin;
        Ok(())
    }

    /// Kill switch: freezes every treasury's spending (trading and fee distributions keep working).
    pub fn set_treasury_frozen(ctx: Context<AdminOnly>, frozen: bool) -> Result<()> {
        ctx.accounts.config.treasury_frozen = frozen;
        Ok(())
    }

    /// Registers an artist whose pump.fun coin the creator just launched (same transaction): quoted in SOL, its
    /// creator this token's vault. Co-signed by the operator; the creator pays the launch fee and the rent.
    pub fn register_artist(ctx: Context<RegisterArtist>) -> Result<()> {
        let curve = read_curve(&ctx.accounts.bonding_curve, &ctx.accounts.mint.key())?;
        require_keys_eq!(curve.creator, ctx.accounts.vault.key(), LaunchpadError::WrongCurve);
        require!(curve.quote_mint == Pubkey::default() || curve.quote_mint == WSOL, LaunchpadError::WrongCurve);
        let fee = ctx.accounts.config.artist_launch_fee;
        pay_launch(&ctx.accounts.creator, &ctx.accounts.platform_wallet, &ctx.accounts.vault, &ctx.accounts.system_program, fee)?;

        let artist = &mut ctx.accounts.artist;
        artist.creator = ctx.accounts.creator.key();
        artist.mint = ctx.accounts.mint.key();
        artist.song_count = 0;
        artist.spend_day = 0;
        artist.spent_today = 0;
        artist.bump = ctx.bumps.artist;
        artist.treasury_bump = ctx.bumps.treasury;
        artist.vault_bump = ctx.bumps.vault;
        emit!(ArtistRegistered { artist: artist.key(), mint: artist.mint, creator: artist.creator, vault: ctx.accounts.vault.key() });
        Ok(())
    }

    /// Registers a song the artist's creator just launched (same transaction): a pump.fun coin quoted in the artist's
    /// coin, its creator this token's vault, at least the platform's creator fee rate. Creator + operator.
    pub fn register_song(ctx: Context<RegisterSong>) -> Result<()> {
        let config = &ctx.accounts.config;
        let curve = read_curve(&ctx.accounts.bonding_curve, &ctx.accounts.mint.key())?;
        require_keys_eq!(curve.creator, ctx.accounts.vault.key(), LaunchpadError::WrongCurve);
        require_keys_eq!(curve.quote_mint, ctx.accounts.artist.mint, LaunchpadError::WrongCurve);
        require!(curve.creator_fee_bps >= config.song_creator_fee_bps as u64, LaunchpadError::WrongCurve);
        let fee = config.song_launch_fee;
        pay_launch(&ctx.accounts.creator, &ctx.accounts.platform_wallet, &ctx.accounts.vault, &ctx.accounts.system_program, fee)?;

        let artist = &mut ctx.accounts.artist;
        let index = artist.song_count;
        artist.song_count = index.checked_add(1).ok_or(LaunchpadError::MathOverflow)?;
        let song = &mut ctx.accounts.song;
        song.artist = artist.mint;
        song.mint = ctx.accounts.mint.key();
        song.index = index;
        song.bump = ctx.bumps.song;
        song.vault_bump = ctx.bumps.vault;
        emit!(SongRegistered { artist_mint: artist.mint, mint: song.mint, index, vault: ctx.accounts.vault.key() });
        Ok(())
    }

    /// Splits what an artist's vault collected from pump.fun (SOL on the curve, wrapped SOL from PumpSwap): 30%
    /// platform, 40% the artist's creator, 30% its treasury. Permissionless.
    pub fn distribute_artist(ctx: Context<DistributeArtist>) -> Result<()> {
        let mint = ctx.accounts.artist.mint;
        let seeds: &[&[&[u8]]] = &[&[VAULT_SEED, mint.as_ref(), &[ctx.accounts.artist.vault_bump]]];
        // PumpSwap pays a SOL-quoted coin's creator in wrapped SOL: unwrap it into the vault first
        if let Some(vault_wsol) = &ctx.accounts.vault_wsol {
            require_keys_eq!(vault_wsol.owner, ctx.accounts.vault.key(), LaunchpadError::WrongAccount);
            require_keys_eq!(vault_wsol.mint, WSOL, LaunchpadError::WrongAccount);
            close_account(CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                CloseAccount { account: vault_wsol.to_account_info(), destination: ctx.accounts.vault.to_account_info(), authority: ctx.accounts.vault.to_account_info() },
                seeds,
            ))?;
        }
        let keep = Rent::get()?.minimum_balance(0);
        let total = ctx.accounts.vault.lamports().saturating_sub(keep);
        let (platform, creator, treasury) = ctx.accounts.config.split(total);
        let a = &ctx.accounts;
        for (to, amount) in [(a.platform_wallet.to_account_info(), platform), (a.creator.to_account_info(), creator), (a.treasury_wsol.to_account_info(), treasury)] {
            if amount == 0 {
                continue;
            }
            system_program::transfer(
                CpiContext::new_with_signer(a.system_program.key(), system_program::Transfer { from: a.vault.to_account_info(), to }, seeds),
                amount,
            )?;
        }
        if treasury > 0 {
            sync_native(CpiContext::new(ctx.accounts.token_program.key(), SyncNative { account: ctx.accounts.treasury_wsol.to_account_info() }))?;
        }
        emit!(FeesDistributed { mint, platform, creator, treasury, burnt: 0 });
        Ok(())
    }

    /// Splits what a song's vault collected from pump.fun, in the artist's coin: 30% platform, 40% the artist's
    /// creator, and the treasury's 30% is burnt. Permissionless.
    pub fn distribute_song(ctx: Context<DistributeSong>) -> Result<()> {
        let song = &ctx.accounts.song;
        let seeds: &[&[&[u8]]] = &[&[VAULT_SEED, song.mint.as_ref(), &[song.vault_bump]]];
        let total = ctx.accounts.vault_tokens.amount;
        let (platform, creator, burnt) = ctx.accounts.config.split(total);
        let a = &ctx.accounts;
        for (amount, to) in [(platform, &a.platform_tokens), (creator, &a.creator_tokens)] {
            if amount == 0 {
                continue;
            }
            transfer_checked(
                CpiContext::new_with_signer(
                    a.token_program.key(),
                    TransferChecked { from: a.vault_tokens.to_account_info(), mint: a.artist_mint.to_account_info(), to: to.to_account_info(), authority: a.vault.to_account_info() },
                    seeds,
                ),
                amount,
                a.artist_mint.decimals,
            )?;
        }
        if burnt > 0 {
            burn(
                CpiContext::new_with_signer(
                    a.token_program.key(),
                    Burn { mint: a.artist_mint.to_account_info(), from: a.vault_tokens.to_account_info(), authority: a.vault.to_account_info() },
                    seeds,
                ),
                burnt,
            )?;
        }
        emit!(FeesDistributed { mint: song.mint, platform, creator, treasury: 0, burnt });
        Ok(())
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

/// The launch fee to the platform, and the vault's rent: pump.fun pays SOL creator fees into it as lamports, which an
/// empty account could not take below rent exemption.
fn pay_launch<'info>(creator: &Signer<'info>, platform: &UncheckedAccount<'info>, vault: &SystemAccount<'info>, system: &Program<'info, System>, fee: u64) -> Result<()> {
    if fee > 0 {
        system_program::transfer(CpiContext::new(system.key(), system_program::Transfer { from: creator.to_account_info(), to: platform.to_account_info() }), fee)?;
    }
    let rent = Rent::get()?.minimum_balance(0).saturating_sub(vault.lamports());
    if rent > 0 {
        system_program::transfer(CpiContext::new(system.key(), system_program::Transfer { from: creator.to_account_info(), to: vault.to_account_info() }), rent)?;
    }
    Ok(())
}

/// What registration reads of a pump.fun bonding curve.
pub struct Curve {
    pub creator: Pubkey,
    pub quote_mint: Pubkey,
    pub creator_fee_bps: u64,
}

/// The pump.fun bonding curve of `mint`: owned by pump.fun, at its PDA, with the `BondingCurve` layout (8-byte
/// discriminator, 5 × u64 reserves, complete, creator, mayhem, cashback, quote_mint, creator_fee_bps, ...).
fn read_curve(info: &AccountInfo, mint: &Pubkey) -> Result<Curve> {
    require_keys_eq!(*info.owner, PUMP, LaunchpadError::WrongCurve);
    let (expected, _) = Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &PUMP);
    require_keys_eq!(info.key(), expected, LaunchpadError::WrongCurve);
    let data = info.try_borrow_data()?;
    require!(data.len() >= 123 && data[..8] == BONDING_CURVE_DISCRIMINATOR, LaunchpadError::WrongCurve);
    let key = |at: usize| Pubkey::new_from_array(data[at..at + 32].try_into().unwrap());
    Ok(Curve { creator: key(49), quote_mint: key(83), creator_fee_bps: u64::from_le_bytes(data[115..123].try_into().unwrap()) })
}

// ------------------------------------------------------------------------------------------------ state

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug)]
pub struct ConfigParams {
    pub operator: Pubkey,
    pub platform_wallet: Pubkey,
    pub ops_wallet: Pubkey,
    /// Of the collected creator fees: the platform's part and the artist's creator's part; the rest goes to the
    /// artist's treasury (artists) or is burnt (songs). 3000 / 4000 = 30 / 40 / 30.
    pub platform_bps: u16,
    pub creator_bps: u16,
    pub treasury_daily_cap_bps: u16,
    /// Paid by the creator to the platform at each launch (lamports).
    pub artist_launch_fee: u64,
    pub song_launch_fee: u64,
    /// The pump.fun creator fee a song must be launched with (in its artist's coin).
    pub song_creator_fee_bps: u16,
}

impl ConfigParams {
    fn validate(&self) -> Result<()> {
        require!(self.platform_bps as u64 + self.creator_bps as u64 <= BPS, LaunchpadError::InvalidConfig);
        require!(self.treasury_daily_cap_bps as u64 <= BPS, LaunchpadError::InvalidConfig);
        require!(self.song_creator_fee_bps > 0, LaunchpadError::InvalidConfig);
        Ok(())
    }
}

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    /// Backend key: co-signs registrations, spends treasuries for generations.
    pub operator: Pubkey,
    /// Receives the platform's fees and the launch fees.
    pub platform_wallet: Pubkey,
    /// Receives treasury spending (pays the generation providers).
    pub ops_wallet: Pubkey,
    pub platform_bps: u16,
    pub creator_bps: u16,
    pub treasury_daily_cap_bps: u16,
    pub treasury_frozen: bool,
    pub artist_launch_fee: u64,
    pub song_launch_fee: u64,
    pub song_creator_fee_bps: u16,
    pub bump: u8,
}

impl Config {
    fn apply(&mut self, p: &ConfigParams) {
        self.operator = p.operator;
        self.platform_wallet = p.platform_wallet;
        self.ops_wallet = p.ops_wallet;
        self.platform_bps = p.platform_bps;
        self.creator_bps = p.creator_bps;
        self.treasury_daily_cap_bps = p.treasury_daily_cap_bps;
        self.artist_launch_fee = p.artist_launch_fee;
        self.song_launch_fee = p.song_launch_fee;
        self.song_creator_fee_bps = p.song_creator_fee_bps;
    }

    /// (platform, creator, rest) of `total`.
    fn split(&self, total: u64) -> (u64, u64, u64) {
        let platform = (total as u128 * self.platform_bps as u128 / BPS as u128) as u64;
        let creator = (total as u128 * self.creator_bps as u128 / BPS as u128) as u64;
        (platform, creator, total - platform - creator)
    }
}

#[account]
#[derive(InitSpace)]
pub struct Artist {
    pub creator: Pubkey,
    pub mint: Pubkey,
    pub song_count: u32,
    pub spend_day: i64,
    pub spent_today: u64,
    pub bump: u8,
    pub treasury_bump: u8,
    pub vault_bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Song {
    /// The artist's mint.
    pub artist: Pubkey,
    pub mint: Pubkey,
    pub index: u32,
    pub bump: u8,
    pub vault_bump: u8,
}

// ------------------------------------------------------------------------------------------------ accounts

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + Config::INIT_SPACE, seeds = [CONFIG_SEED], bump)]
    pub config: Account<'info, Config>,
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
    /// The token's pump.fun creator.
    #[account(mut, seeds = [VAULT_SEED, mint.key().as_ref()], bump)]
    pub vault: SystemAccount<'info>,
    /// CHECK: the artist coin, checked against its curve.
    pub mint: UncheckedAccount<'info>,
    /// CHECK: its pump.fun bonding curve (owner, address, discriminator, creator and quote checked).
    pub bonding_curve: UncheckedAccount<'info>,
    /// CHECK: checked against the config.
    #[account(mut, address = config.platform_wallet @ LaunchpadError::Unauthorized)]
    pub platform_wallet: UncheckedAccount<'info>,
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
    #[account(mut, seeds = [VAULT_SEED, mint.key().as_ref()], bump)]
    pub vault: SystemAccount<'info>,
    /// CHECK: the song coin, checked against its curve.
    pub mint: UncheckedAccount<'info>,
    /// CHECK: its pump.fun bonding curve (owner, address, discriminator, creator, quote and fee rate checked).
    pub bonding_curve: UncheckedAccount<'info>,
    /// CHECK: checked against the config.
    #[account(mut, address = config.platform_wallet @ LaunchpadError::Unauthorized)]
    pub platform_wallet: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DistributeArtist<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [ARTIST_SEED, artist.mint.as_ref()], bump = artist.bump)]
    pub artist: Box<Account<'info, Artist>>,
    #[account(mut, seeds = [VAULT_SEED, artist.mint.as_ref()], bump = artist.vault_bump)]
    pub vault: SystemAccount<'info>,
    /// The vault's wrapped SOL (PumpSwap creator fees), closed into the vault when given.
    #[account(mut)]
    pub vault_wsol: Option<Box<InterfaceAccount<'info, TokenAccount>>>,
    /// CHECK: checked against the config.
    #[account(mut, address = config.platform_wallet @ LaunchpadError::Unauthorized)]
    pub platform_wallet: UncheckedAccount<'info>,
    /// CHECK: the artist's creator.
    #[account(mut, address = artist.creator @ LaunchpadError::NotArtistCreator)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: the artist's treasury PDA.
    #[account(seeds = [TREASURY_SEED, artist.mint.as_ref()], bump = artist.treasury_bump)]
    pub treasury: UncheckedAccount<'info>,
    #[account(address = WSOL)]
    pub wsol_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(init_if_needed, payer = payer, associated_token::mint = wsol_mint, associated_token::authority = treasury, associated_token::token_program = token_program)]
    pub treasury_wsol: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DistributeSong<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [ARTIST_SEED, artist.mint.as_ref()], bump = artist.bump)]
    pub artist: Box<Account<'info, Artist>>,
    #[account(seeds = [SONG_SEED, artist.mint.as_ref(), &song.index.to_le_bytes()], bump = song.bump, constraint = song.artist == artist.mint @ LaunchpadError::WrongAccount)]
    pub song: Box<Account<'info, Song>>,
    #[account(seeds = [VAULT_SEED, song.mint.as_ref()], bump = song.vault_bump)]
    pub vault: SystemAccount<'info>,
    #[account(mut, address = artist.mint @ LaunchpadError::WrongAccount, mint::token_program = token_program)]
    pub artist_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, associated_token::mint = artist_mint, associated_token::authority = vault, associated_token::token_program = token_program)]
    pub vault_tokens: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: checked against the config.
    #[account(address = config.platform_wallet @ LaunchpadError::Unauthorized)]
    pub platform_wallet: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = payer, associated_token::mint = artist_mint, associated_token::authority = platform_wallet, associated_token::token_program = token_program)]
    pub platform_tokens: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: the artist's creator.
    #[account(address = artist.creator @ LaunchpadError::NotArtistCreator)]
    pub creator: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = payer, associated_token::mint = artist_mint, associated_token::authority = creator, associated_token::token_program = token_program)]
    pub creator_tokens: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
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
    pub creator: Pubkey,
    pub vault: Pubkey,
}

#[event]
pub struct SongRegistered {
    pub artist_mint: Pubkey,
    pub mint: Pubkey,
    pub index: u32,
    pub vault: Pubkey,
}

/// A vault's collected creator fees split: in SOL for an artist (`treasury`), in the artist's coin for a song
/// (`burnt`).
#[event]
pub struct FeesDistributed {
    pub mint: Pubkey,
    pub platform: u64,
    pub creator: u64,
    pub treasury: u64,
    pub burnt: u64,
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
    #[msg("Not a MILK pump.fun curve for this token")]
    WrongCurve,
    #[msg("Wrong account")]
    WrongAccount,
}
