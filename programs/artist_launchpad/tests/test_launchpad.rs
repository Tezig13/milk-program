use {
    anchor_lang::{prelude::Pubkey, solana_program::instruction::Instruction, AccountDeserialize, InstructionData, ToAccountMetas},
    artist_launchpad::{Artist, ConfigParams, Curve, CurveParams, Song, TOTAL_SUPPLY, WSOL},
    litesvm::LiteSVM,
    solana_account::Account,
    solana_clock::Clock,
    solana_keypair::Keypair,
    solana_message::{Message, VersionedMessage},
    solana_program_pack::Pack,
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
    spl_associated_token_account_interface::address::get_associated_token_address_with_program_id as ata,
    spl_token_interface::state::{Account as TokenAccount, AccountState, Mint},
};

const SOL: u64 = 1_000_000_000;
const TOK: u64 = 1_000_000; // 6 decimals
const NOW: i64 = 1_800_000_000;
const TOKEN: Pubkey = spl_token_interface::ID;
const TOKEN_2022: Pubkey = spl_token_2022_interface::ID;
const ATA_PROGRAM: Pubkey = spl_associated_token_account_interface::program::ID;
const SYSTEM: Pubkey = anchor_lang::solana_program::system_program::ID;
const DAMM: Pubkey = artist_launchpad::damm_v2::ID;

const ARTIST_CURVE: CurveParams = CurveParams { virtual_base: 1_073_000_000 * TOK, virtual_quote: 30 * SOL, curve_supply: 793_100_000 * TOK };
const SONG_CURVE: CurveParams = CurveParams { virtual_base: 1_073_000_000 * TOK, virtual_quote: 30_000_000 * TOK, curve_supply: 793_100_000 * TOK };

struct Env {
    svm: LiteSVM,
    admin: Keypair,
    operator: Keypair,
    platform: Pubkey,
    ops: Pubkey,
}

struct Launched {
    mint: Pubkey,
    curve: Pubkey,
}

fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &artist_launchpad::id()).0
}

fn send(svm: &mut LiteSVM, ixs: &[Instruction], signers: &[&Keypair]) -> Result<(), String> {
    svm.expire_blockhash();
    let msg = Message::new_with_blockhash(ixs, Some(&signers[0].pubkey()), &svm.latest_blockhash());
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), signers).unwrap();
    svm.send_transaction(tx).map(|_| ()).map_err(|e| format!("{:?}\n{}", e.err, e.meta.logs.join("\n")))
}

fn warp(svm: &mut LiteSVM, seconds: i64) {
    let mut clock = svm.get_sysvar::<Clock>();
    clock.unix_timestamp += seconds;
    svm.set_sysvar(&clock);
}

fn amount(svm: &LiteSVM, key: &Pubkey) -> u64 {
    TokenAccount::unpack(&svm.get_account(key).unwrap().data[..TokenAccount::LEN]).unwrap().amount
}

fn jo_supply(svm: &LiteSVM, mint: &Pubkey) -> u64 {
    Mint::unpack(&svm.get_account(mint).unwrap().data[..Mint::LEN]).unwrap().supply
}

fn curve_of(svm: &LiteSVM, mint: &Pubkey) -> Curve {
    Curve::try_deserialize(&mut &svm.get_account(&pda(&[b"curve", mint.as_ref()])).unwrap().data[..]).unwrap()
}

fn funded(svm: &mut LiteSVM) -> Keypair {
    let k = Keypair::new();
    svm.airdrop(&k.pubkey(), 1_000 * SOL).unwrap();
    k
}

/// A wrapped-SOL account holding `lamports` worth of WSOL, like a wallet that wrapped its SOL.
fn give_wsol(svm: &mut LiteSVM, owner: &Pubkey, lamports: u64) -> Pubkey {
    let key = ata(owner, &WSOL, &TOKEN);
    let rent = svm.minimum_balance_for_rent_exemption(TokenAccount::LEN);
    let mut data = vec![0u8; TokenAccount::LEN];
    TokenAccount {
        mint: WSOL,
        owner: *owner,
        amount: lamports,
        delegate: None.into(),
        state: AccountState::Initialized,
        is_native: Some(rent).into(),
        delegated_amount: 0,
        close_authority: None.into(),
    }
    .pack_into_slice(&mut data);
    svm.set_account(key, Account { lamports: rent + lamports, data, owner: TOKEN, executable: false, rent_epoch: 0 }).unwrap();
    key
}

fn params(env_operator: Pubkey, platform: Pubkey, ops: Pubkey) -> ConfigParams {
    ConfigParams {
        operator: env_operator,
        platform_wallet: platform,
        ops_wallet: ops,
        fee_bps: 100,
        platform_share_bps: 3000,
        creator_share_bps: 4000,
        treasury_daily_cap_bps: 2000,
        artist_curve: ARTIST_CURVE,
        song_curve: SONG_CURVE,
    }
}

fn setup() -> Env {
    let mut svm = LiteSVM::new();
    svm.add_program(artist_launchpad::id(), include_bytes!(concat!(env!("CARGO_TARGET_TMPDIR"), "/../deploy/artist_launchpad.so"))).unwrap();
    // Meteora DAMM v2 (cp-amm), the program deployed on mainnet (dumped into tests/fixtures).
    svm.add_program(DAMM, include_bytes!("../../../tests/fixtures/cp_amm.so")).unwrap();
    let mut clock = svm.get_sysvar::<Clock>();
    clock.unix_timestamp = NOW;
    svm.set_sysvar(&clock);
    // The native mint, as on mainnet.
    let mut data = vec![0u8; Mint::LEN];
    Mint { mint_authority: None.into(), supply: 0, decimals: 9, is_initialized: true, freeze_authority: None.into() }.pack_into_slice(&mut data);
    svm.set_account(WSOL, Account { lamports: 1_000_000_000, data, owner: TOKEN, executable: false, rent_epoch: 0 }).unwrap();

    let admin = Keypair::new_from_array([9u8; 32]);
    svm.airdrop(&admin.pubkey(), 100 * SOL).unwrap();
    let operator = funded(&mut svm);
    let platform = Pubkey::new_unique();
    let ops = Pubkey::new_unique();
    let ix = Instruction::new_with_bytes(
        artist_launchpad::id(),
        &artist_launchpad::instruction::InitializeConfig { params: params(operator.pubkey(), platform, ops) }.data(),
        artist_launchpad::accounts::InitializeConfig { admin: admin.pubkey(), config: pda(&[b"config"]), system_program: SYSTEM }.to_account_metas(None),
    );
    send(&mut svm, &[ix], &[&admin]).unwrap();
    Env { svm, admin, operator, platform, ops }
}

fn create_artist_ix(env: &Env, creator: &Pubkey, mint: &Pubkey, operator: &Pubkey, name: &str) -> Instruction {
    let curve = pda(&[b"curve", mint.as_ref()]);
    Instruction::new_with_bytes(
        artist_launchpad::id(),
        &artist_launchpad::instruction::CreateArtist { name: name.into(), symbol: name.to_uppercase(), uri: format!("https://cdn.example/{name}.json") }.data(),
        artist_launchpad::accounts::CreateArtist {
            creator: *creator,
            operator: *operator,
            config: pda(&[b"config"]),
            mint: *mint,
            artist: pda(&[b"artist", mint.as_ref()]),
            curve,
            treasury: pda(&[b"treasury", mint.as_ref()]),
            base_vault: ata(&curve, mint, &TOKEN_2022),
            quote_mint: WSOL,
            quote_vault: ata(&curve, &WSOL, &TOKEN),
            token_program: TOKEN_2022,
            quote_token_program: TOKEN,
            associated_token_program: ATA_PROGRAM,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
    )
    .tap_env(env)
}

trait TapEnv {
    fn tap_env(self, _env: &Env) -> Self;
}
impl TapEnv for Instruction {
    fn tap_env(self, _env: &Env) -> Self {
        self
    }
}

fn launch_artist_unarmed(env: &mut Env, creator: &Keypair, name: &str) -> Launched {
    let mint = Keypair::new();
    let ix = create_artist_ix(env, &creator.pubkey(), &mint.pubkey(), &env.operator.pubkey(), name);
    let op = env.operator.insecure_clone();
    send(&mut env.svm, &[ix], &[creator, &mint, &op]).unwrap();
    Launched { mint: mint.pubkey(), curve: pda(&[b"curve", mint.pubkey().as_ref()]) }
}

/// Launch + the pool armed right after, as the platform does it.
fn launch_artist(env: &mut Env, creator: &Keypair, name: &str) -> Launched {
    let l = launch_artist_unarmed(env, creator, name);
    let payer = funded(&mut env.svm);
    arm(env, &payer, &l.mint).unwrap();
    l
}

fn create_song_ix(creator: &Pubkey, operator: &Pubkey, artist_mint: &Pubkey, index: u32, mint: &Pubkey, name: &str) -> Instruction {
    let curve = pda(&[b"curve", mint.as_ref()]);
    Instruction::new_with_bytes(
        artist_launchpad::id(),
        &artist_launchpad::instruction::CreateSong { name: name.into(), symbol: name.to_uppercase(), uri: format!("https://cdn.example/{name}.json") }.data(),
        artist_launchpad::accounts::CreateSong {
            creator: *creator,
            operator: *operator,
            config: pda(&[b"config"]),
            artist: pda(&[b"artist", artist_mint.as_ref()]),
            mint: *mint,
            song: pda(&[b"song", artist_mint.as_ref(), &index.to_le_bytes()]),
            curve,
            base_vault: ata(&curve, mint, &TOKEN_2022),
            quote_mint: *artist_mint,
            quote_vault: ata(&curve, artist_mint, &TOKEN_2022),
            token_program: TOKEN_2022,
            associated_token_program: ATA_PROGRAM,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
    )
}

fn launch_song(env: &mut Env, creator: &Keypair, artist_mint: &Pubkey, index: u32, name: &str) -> Launched {
    let mint = Keypair::new();
    let ix = create_song_ix(&creator.pubkey(), &env.operator.pubkey(), artist_mint, index, &mint.pubkey(), name);
    let op = env.operator.insecure_clone();
    send(&mut env.svm, &[ix], &[creator, &mint, &op]).unwrap();
    let payer = funded(&mut env.svm);
    arm(env, &payer, &mint.pubkey()).unwrap();
    Launched { mint: mint.pubkey(), curve: pda(&[b"curve", mint.pubkey().as_ref()]) }
}

/// (quote mint, quote token program) of a curve.
fn quote_of(svm: &LiteSVM, mint: &Pubkey) -> (Pubkey, Pubkey) {
    let c = curve_of(svm, mint);
    if c.quote_mint == WSOL { (WSOL, TOKEN) } else { (c.quote_mint, TOKEN_2022) }
}

fn trade_ix(svm: &LiteSVM, trader: &Pubkey, mint: &Pubkey, buy: bool, amount_in: u64, min_out: u64) -> Instruction {
    let curve = pda(&[b"curve", mint.as_ref()]);
    let (quote, quote_program) = quote_of(svm, mint);
    let data = if buy {
        artist_launchpad::instruction::Buy { quote_in: amount_in, min_base_out: min_out }.data()
    } else {
        artist_launchpad::instruction::Sell { base_in: amount_in, min_quote_out: min_out }.data()
    };
    Instruction::new_with_bytes(
        artist_launchpad::id(),
        &data,
        artist_launchpad::accounts::Trade {
            trader: *trader,
            curve,
            base_mint: *mint,
            quote_mint: quote,
            base_vault: ata(&curve, mint, &TOKEN_2022),
            quote_vault: ata(&curve, &quote, &quote_program),
            trader_base: ata(trader, mint, &TOKEN_2022),
            trader_quote: ata(trader, &quote, &quote_program),
            base_token_program: TOKEN_2022,
            quote_token_program: quote_program,
            associated_token_program: ATA_PROGRAM,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
    )
}

fn buy(env: &mut Env, trader: &Keypair, mint: &Pubkey, quote_in: u64) -> Result<(), String> {
    let ix = trade_ix(&env.svm, &trader.pubkey(), mint, true, quote_in, 0);
    send(&mut env.svm, &[ix], &[trader])
}

fn sell(env: &mut Env, trader: &Keypair, mint: &Pubkey, base_in: u64) -> Result<(), String> {
    let ix = trade_ix(&env.svm, &trader.pubkey(), mint, false, base_in, 0);
    send(&mut env.svm, &[ix], &[trader])
}

fn claim_ix(env: &Env, payer: &Pubkey, mint: &Pubkey, creator: &Pubkey) -> Instruction {
    let c = curve_of(&env.svm, mint);
    let (quote, qp) = quote_of(&env.svm, mint);
    let curve = pda(&[b"curve", mint.as_ref()]);
    let treasury = pda(&[b"treasury", c.artist_mint.as_ref()]);
    Instruction::new_with_bytes(
        artist_launchpad::id(),
        &artist_launchpad::instruction::ClaimFees {}.data(),
        artist_launchpad::accounts::ClaimFees {
            payer: *payer,
            config: pda(&[b"config"]),
            curve,
            artist: pda(&[b"artist", c.artist_mint.as_ref()]),
            treasury,
            quote_mint: quote,
            quote_vault: ata(&curve, &quote, &qp),
            platform_wallet: env.platform,
            platform_quote: ata(&env.platform, &quote, &qp),
            creator: *creator,
            creator_quote: ata(creator, &quote, &qp),
            treasury_quote: ata(&treasury, &quote, &qp),
            quote_token_program: qp,
            associated_token_program: ATA_PROGRAM,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
    )
}

fn spend_ix(env: &Env, operator: &Pubkey, artist_mint: &Pubkey, amount: u64, ops_quote: Pubkey) -> Instruction {
    let treasury = pda(&[b"treasury", artist_mint.as_ref()]);
    Instruction::new_with_bytes(
        artist_launchpad::id(),
        &artist_launchpad::instruction::TreasurySpend { amount, memo: "suno song".into() }.data(),
        artist_launchpad::accounts::TreasurySpend {
            operator: *operator,
            config: pda(&[b"config"]),
            artist: pda(&[b"artist", artist_mint.as_ref()]),
            treasury,
            quote_mint: WSOL,
            treasury_quote: ata(&treasury, &WSOL, &TOKEN),
            ops_quote,
            quote_token_program: TOKEN,
        }
        .to_account_metas(None),
    )
    .tap_env(env)
}

// ------------------------------------------------------------------------------------------------ tests

#[test]
fn only_the_initializer_can_create_the_config() {
    let mut svm = LiteSVM::new();
    svm.add_program(artist_launchpad::id(), include_bytes!(concat!(env!("CARGO_TARGET_TMPDIR"), "/../deploy/artist_launchpad.so"))).unwrap();
    let intruder = Keypair::new();
    svm.airdrop(&intruder.pubkey(), 10 * SOL).unwrap();
    let ix = Instruction::new_with_bytes(
        artist_launchpad::id(),
        &artist_launchpad::instruction::InitializeConfig { params: params(Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()) }.data(),
        artist_launchpad::accounts::InitializeConfig { admin: intruder.pubkey(), config: pda(&[b"config"]), system_program: SYSTEM }.to_account_metas(None),
    );
    assert!(send(&mut svm, &[ix], &[&intruder]).unwrap_err().contains("Unauthorized"));
}

#[test]
fn an_artist_token_is_minted_once_with_its_metadata() {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist_unarmed(&mut env, &creator, "jo");
    let vault = ata(&jo.curve, &jo.mint, &TOKEN_2022);
    assert_eq!(amount(&env.svm, &vault), TOTAL_SUPPLY, "the whole supply sits on the curve");
    let mint_data = env.svm.get_account(&jo.mint).unwrap().data;
    let mint = Mint::unpack(&mint_data[..Mint::LEN]).unwrap();
    assert_eq!(mint.supply, TOTAL_SUPPLY);
    assert!(mint.mint_authority.is_none(), "nobody can ever mint more");
    let text = String::from_utf8_lossy(&mint_data);
    assert!(text.contains("JO") && text.contains("https://cdn.example/jo.json"), "name, symbol, uri in the mint");
    let artist = Artist::try_deserialize(&mut &env.svm.get_account(&pda(&[b"artist", jo.mint.as_ref()])).unwrap().data[..]).unwrap();
    assert_eq!(artist.creator, creator.pubkey());
    let c = curve_of(&env.svm, &jo.mint);
    assert_eq!((c.quote_mint, c.real_base, c.kind), (WSOL, ARTIST_CURVE.curve_supply, 0));
}

#[test]
fn launches_need_the_operator() {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let fake_operator = funded(&mut env.svm);
    let mint = Keypair::new();
    let ix = create_artist_ix(&env, &creator.pubkey(), &mint.pubkey(), &fake_operator.pubkey(), "jo");
    assert!(send(&mut env.svm, &[ix], &[&creator, &mint, &fake_operator]).unwrap_err().contains("Unauthorized"));
}

#[test]
fn buying_and_selling_the_artist_token_in_sol() {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist(&mut env, &creator, "jo");
    let fan = funded(&mut env.svm);
    let fan_wsol = give_wsol(&mut env.svm, &fan.pubkey(), 10 * SOL);

    buy(&mut env, &fan, &jo.mint, SOL).unwrap();
    let got = amount(&env.svm, &ata(&fan.pubkey(), &jo.mint, &TOKEN_2022));
    // 1% fee, then x·y = k on virtual reserves: 1.073B × 0.99 / (30 + 0.99)
    let expected = (ARTIST_CURVE.virtual_base as u128 * 990_000_000 / (ARTIST_CURVE.virtual_quote as u128 + 990_000_000)) as u64;
    assert_eq!(got, expected);
    assert_eq!(amount(&env.svm, &fan_wsol), 9 * SOL);
    let c = curve_of(&env.svm, &jo.mint);
    assert_eq!(c.real_quote, 990_000_000);
    assert_eq!((c.fees_platform, c.fees_creator, c.fees_treasury), (3_000_000, 4_000_000, 3_000_000), "fee split 30/40/30");

    sell(&mut env, &fan, &jo.mint, got).unwrap();
    let back = amount(&env.svm, &fan_wsol) - 9 * SOL;
    assert!(back < SOL && back > 978_000_000, "round trip costs the two 1% fees: got back {back}");
    assert_eq!(amount(&env.svm, &ata(&fan.pubkey(), &jo.mint, &TOKEN_2022)), 0);
}

#[test]
fn slippage_protects_the_trader() {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist(&mut env, &creator, "jo");
    let fan = funded(&mut env.svm);
    give_wsol(&mut env.svm, &fan.pubkey(), 10 * SOL);
    let ix = trade_ix(&env.svm, &fan.pubkey(), &jo.mint, true, SOL, 40_000_000 * TOK);
    assert!(send(&mut env.svm, &[ix], &[&fan]).unwrap_err().contains("Slippage"));
}

#[test]
fn the_curve_completes_when_sold_out() {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist(&mut env, &creator, "jo");
    let whale = funded(&mut env.svm);
    let wsol = give_wsol(&mut env.svm, &whale.pubkey(), 500 * SOL);
    buy(&mut env, &whale, &jo.mint, 200 * SOL).unwrap();
    let c = curve_of(&env.svm, &jo.mint);
    assert!(c.complete && c.real_base == 0);
    assert_eq!(amount(&env.svm, &ata(&whale.pubkey(), &jo.mint, &TOKEN_2022)), ARTIST_CURVE.curve_supply, "the last buy gets what is left");
    let paid = 500 * SOL - amount(&env.svm, &wsol);
    assert!(paid < 200 * SOL, "and only pays for it: {} SOL", paid as f64 / 1e9);
    // ≈ 85 SOL raised (30 virtual SOL, pump-style)
    assert!(c.real_quote > 84 * SOL && c.real_quote < 86 * SOL, "raised {} SOL", c.real_quote as f64 / 1e9);
    let reserve = amount(&env.svm, &ata(&jo.curve, &jo.mint, &TOKEN_2022));
    let reserved = TOTAL_SUPPLY - ARTIST_CURVE.curve_supply - artist_launchpad::SONG_SEED_FUND;
    assert!(reserve <= reserved && reserved - reserve < 5_000 * TOK, "the pool share stays for graduation (minus the armed pool's dust): {reserve}");
    assert!(buy(&mut env, &whale, &jo.mint, SOL).unwrap_err().contains("CurveComplete"));
    assert!(sell(&mut env, &whale, &jo.mint, TOK).unwrap_err().contains("CurveComplete"));
}

#[test]
fn songs_are_paired_in_the_artist_token() {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist(&mut env, &creator, "jo");
    let a = launch_song(&mut env, &creator, &jo.mint, 0, "a");
    let b = launch_song(&mut env, &creator, &jo.mint, 1, "b");
    let ca = curve_of(&env.svm, &a.mint);
    assert_eq!((ca.quote_mint, ca.artist_mint, ca.kind), (jo.mint, jo.mint, 1), "$A/$JO");
    assert_eq!(curve_of(&env.svm, &b.mint).quote_mint, jo.mint, "$B/$JO");
    let song = Song::try_deserialize(&mut &env.svm.get_account(&pda(&[b"song", jo.mint.as_ref(), &1u32.to_le_bytes()])).unwrap().data[..]).unwrap();
    assert_eq!((song.mint, song.index), (b.mint, 1));

    // A fan buys $JO with SOL, then $A with $JO.
    let fan = funded(&mut env.svm);
    give_wsol(&mut env.svm, &fan.pubkey(), 20 * SOL);
    buy(&mut env, &fan, &jo.mint, 10 * SOL).unwrap();
    let jo_bal = amount(&env.svm, &ata(&fan.pubkey(), &jo.mint, &TOKEN_2022));
    let spend = 10_000_000 * TOK; // 10M $JO
    buy(&mut env, &fan, &a.mint, spend).unwrap();
    assert!(amount(&env.svm, &ata(&fan.pubkey(), &a.mint, &TOKEN_2022)) > 0);
    assert_eq!(amount(&env.svm, &ata(&fan.pubkey(), &jo.mint, &TOKEN_2022)), jo_bal - spend, "paid in $JO");
    assert_eq!(curve_of(&env.svm, &a.mint).real_quote, spend - spend / 100, "the $A/$JO curve holds the $JO");
    let fees = curve_of(&env.svm, &a.mint);
    assert!(fees.fees_creator > 0, "song fees accrue in $JO");
}

#[test]
fn only_the_artist_creator_releases_songs() {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist(&mut env, &creator, "jo");
    let stranger = funded(&mut env.svm);
    let mint = Keypair::new();
    let ix = create_song_ix(&stranger.pubkey(), &env.operator.pubkey(), &jo.mint, 0, &mint.pubkey(), "fake");
    let op = env.operator.insecure_clone();
    assert!(send(&mut env.svm, &[ix], &[&stranger, &mint, &op]).unwrap_err().contains("NotArtistCreator"));
}

#[test]
fn claimed_fees_go_30_40_30() {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist(&mut env, &creator, "jo");
    let a = launch_song(&mut env, &creator, &jo.mint, 0, "a");
    let fan = funded(&mut env.svm);
    give_wsol(&mut env.svm, &fan.pubkey(), 50 * SOL);
    buy(&mut env, &fan, &jo.mint, 10 * SOL).unwrap();
    let jo_bal = amount(&env.svm, &ata(&fan.pubkey(), &jo.mint, &TOKEN_2022));
    buy(&mut env, &fan, &a.mint, jo_bal).unwrap();

    // Artist curve: fees in SOL.
    let anyone = funded(&mut env.svm);
    let ix = claim_ix(&env, &anyone.pubkey(), &jo.mint, &creator.pubkey());
    send(&mut env.svm, &[ix], &[&anyone]).unwrap();
    let treasury = pda(&[b"treasury", jo.mint.as_ref()]);
    assert_eq!(amount(&env.svm, &ata(&env.platform, &WSOL, &TOKEN)), 30_000_000, "platform 30%");
    assert_eq!(amount(&env.svm, &ata(&creator.pubkey(), &WSOL, &TOKEN)), 40_000_000, "creator 40%");
    assert_eq!(amount(&env.svm, &ata(&treasury, &WSOL, &TOKEN)), 30_000_000, "artist treasury 30%");
    let c = curve_of(&env.svm, &jo.mint);
    assert_eq!(c.fees_platform + c.fees_creator + c.fees_treasury, 0);
    assert_eq!(amount(&env.svm, &ata(&jo.curve, &WSOL, &TOKEN)), c.real_quote, "the vault keeps exactly the reserves");

    // Song curve: fees in $JO; platform and creator get theirs, the treasury's 30% is burned.
    let fee = curve_of(&env.svm, &a.mint);
    let supply_before = jo_supply(&env.svm, &jo.mint);
    let seed_fund = amount(&env.svm, &ata(&treasury, &jo.mint, &TOKEN_2022));
    let ix = claim_ix(&env, &anyone.pubkey(), &a.mint, &creator.pubkey());
    send(&mut env.svm, &[ix], &[&anyone]).unwrap();
    assert!(fee.fees_treasury > 0);
    assert_eq!(amount(&env.svm, &ata(&env.platform, &jo.mint, &TOKEN_2022)), fee.fees_platform, "platform 30% in $JO");
    assert_eq!(amount(&env.svm, &ata(&creator.pubkey(), &jo.mint, &TOKEN_2022)), fee.fees_creator, "creator 40% in $JO");
    assert_eq!(amount(&env.svm, &ata(&treasury, &jo.mint, &TOKEN_2022)), seed_fund, "the treasury gets no $JO from fees (it only keeps its song seed fund)");
    assert_eq!(supply_before - jo_supply(&env.svm, &jo.mint), fee.fees_treasury, "the treasury's 30% of a song's fees is burned");
    let c = curve_of(&env.svm, &a.mint);
    assert_eq!(amount(&env.svm, &ata(&a.curve, &jo.mint, &TOKEN_2022)), c.real_quote, "the song vault keeps exactly the reserves");
    let redirect = { let ix = claim_ix(&env, &anyone.pubkey(), &a.mint, &env.platform); send(&mut env.svm, &[ix], &[&anyone]) };
    assert!(redirect.is_err(), "fees cannot be redirected to another wallet than the artist's creator");
}

#[test]
fn treasury_pays_generations_within_a_daily_cap() {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist(&mut env, &creator, "jo");
    let fan = funded(&mut env.svm);
    give_wsol(&mut env.svm, &fan.pubkey(), 200 * SOL);
    buy(&mut env, &fan, &jo.mint, 100 * SOL).unwrap(); // completes the curve; fees accrue
    let anyone = funded(&mut env.svm);
    { let ix = claim_ix(&env, &anyone.pubkey(), &jo.mint, &creator.pubkey()); send(&mut env.svm, &[ix], &[&anyone]) }.unwrap();
    let treasury = pda(&[b"treasury", jo.mint.as_ref()]);
    let held = amount(&env.svm, &ata(&treasury, &WSOL, &TOKEN));
    let ops_wsol = give_wsol(&mut env.svm, &env.ops.clone(), 0);

    let cap = held / 5; // 20% a day
    let op = env.operator.insecure_clone();
    { let ix = spend_ix(&env, &op.pubkey(), &jo.mint, cap / 2, ops_wsol); send(&mut env.svm, &[ix], &[&op]) }.unwrap();
    { let ix = spend_ix(&env, &op.pubkey(), &jo.mint, cap / 2, ops_wsol); send(&mut env.svm, &[ix], &[&op]) }.unwrap();
    assert!({ let ix = spend_ix(&env, &op.pubkey(), &jo.mint, 1_000, ops_wsol); send(&mut env.svm, &[ix], &[&op]) }.unwrap_err().contains("DailyCapExceeded"));
    assert_eq!(amount(&env.svm, &ops_wsol), cap / 2 * 2);

    warp(&mut env.svm, 86_400);
    { let ix = spend_ix(&env, &op.pubkey(), &jo.mint, 1_000, ops_wsol); send(&mut env.svm, &[ix], &[&op]) }.unwrap();

    // Only the operator; and the admin's kill switch stops it.
    let stranger = funded(&mut env.svm);
    assert!({ let ix = spend_ix(&env, &stranger.pubkey(), &jo.mint, 1_000, ops_wsol); send(&mut env.svm, &[ix], &[&stranger]) }.unwrap_err().contains("Unauthorized"));
    let freeze = Instruction::new_with_bytes(
        artist_launchpad::id(),
        &artist_launchpad::instruction::SetTreasuryFrozen { frozen: true }.data(),
        artist_launchpad::accounts::AdminOnly { admin: env.admin.pubkey(), config: pda(&[b"config"]) }.to_account_metas(None),
    );
    let admin = env.admin.insecure_clone();
    send(&mut env.svm, &[freeze], &[&admin]).unwrap();
    assert!({ let ix = spend_ix(&env, &op.pubkey(), &jo.mint, 1_000, ops_wsol); send(&mut env.svm, &[ix], &[&op]) }.unwrap_err().contains("TreasuryFrozen"));
    // Spending only ever goes to the platform's ops wallet.
    let elsewhere = give_wsol(&mut env.svm, &stranger.pubkey(), 0);
    let unfreeze = Instruction::new_with_bytes(
        artist_launchpad::id(),
        &artist_launchpad::instruction::SetTreasuryFrozen { frozen: false }.data(),
        artist_launchpad::accounts::AdminOnly { admin: env.admin.pubkey(), config: pda(&[b"config"]) }.to_account_metas(None),
    );
    send(&mut env.svm, &[unfreeze], &[&admin]).unwrap();
    assert!({ let ix = spend_ix(&env, &op.pubkey(), &jo.mint, 1_000, elsewhere); send(&mut env.svm, &[ix], &[&op]) }.is_err());
}

#[test]
fn only_the_admin_changes_the_config() {
    let mut env = setup();
    let stranger = funded(&mut env.svm);
    let mut p = params(env.operator.pubkey(), env.platform, env.ops);
    let ix = |admin: Pubkey, p: ConfigParams| {
        Instruction::new_with_bytes(
            artist_launchpad::id(),
            &artist_launchpad::instruction::UpdateConfig { params: p }.data(),
            artist_launchpad::accounts::AdminOnly { admin, config: pda(&[b"config"]) }.to_account_metas(None),
        )
    };
    assert!(send(&mut env.svm, &[ix(stranger.pubkey(), p)], &[&stranger]).unwrap_err().contains("Unauthorized"));
    p.fee_bps = 600;
    let admin = env.admin.insecure_clone();
    assert!(send(&mut env.svm, &[ix(admin.pubkey(), p)], &[&admin]).unwrap_err().contains("InvalidConfig"), "fee capped at 5%");
}

// ------------------------------------------------------------------------------------------------ graduation (DAMM v2)

fn damm_pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &DAMM).0
}

fn compute_budget(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction::new_with_bytes(Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"), &data, vec![])
}

#[derive(Debug)]
struct PoolKeys {
    pool: Pubkey,
    position: Pubkey,
    nft_account: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
}

fn pool_keys(mint: &Pubkey, quote: &Pubkey, nft_mint: &Pubkey) -> PoolKeys {
    let (hi, lo) = if mint.to_bytes() > quote.to_bytes() { (mint, quote) } else { (quote, mint) };
    let pool = damm_pda(&[b"cpool", hi.as_ref(), lo.as_ref()]);
    PoolKeys {
        pool,
        position: damm_pda(&[b"position", nft_mint.as_ref()]),
        nft_account: damm_pda(&[b"position_nft_account", nft_mint.as_ref()]),
        base_vault: damm_pda(&[b"token_vault", mint.as_ref(), pool.as_ref()]),
        quote_vault: damm_pda(&[b"token_vault", quote.as_ref(), pool.as_ref()]),
    }
}

/// Arms a token's DAMM v2 pool (dust liquidity at the graduation price, owned by the program).
fn arm(env: &mut Env, payer: &Keypair, mint: &Pubkey) -> Result<PoolKeys, String> {
    let (quote, qp) = quote_of(&env.svm, mint);
    let c = curve_of(&env.svm, mint);
    let curve = pda(&[b"curve", mint.as_ref()]);
    let migrator = pda(&[b"migrator", mint.as_ref()]);
    let treasury = pda(&[b"treasury", c.artist_mint.as_ref()]);
    let nft = Keypair::new();
    let k = pool_keys(mint, &quote, &nft.pubkey());
    let ix = Instruction::new_with_bytes(
        artist_launchpad::id(),
        &artist_launchpad::instruction::ArmPool {}.data(),
        artist_launchpad::accounts::ArmPool {
            payer: payer.pubkey(),
            config: pda(&[b"config"]),
            curve,
            artist: pda(&[b"artist", c.artist_mint.as_ref()]),
            treasury,
            artist_mint: c.artist_mint,
            treasury_artist_tokens: ata(&treasury, &c.artist_mint, &TOKEN_2022),
            migrator,
            base_mint: *mint,
            quote_mint: quote,
            base_vault: ata(&curve, mint, &TOKEN_2022),
            migrator_base: ata(&migrator, mint, &TOKEN_2022),
            migrator_quote: ata(&migrator, &quote, &qp),
            position_nft_mint: nft.pubkey(),
            position_nft_account: k.nft_account,
            pool_authority: damm_pda(&[b"pool_authority"]),
            pool: k.pool,
            position: k.position,
            pool_base_vault: k.base_vault,
            pool_quote_vault: k.quote_vault,
            base_token_program: TOKEN_2022,
            quote_token_program: qp,
            token_2022_program: TOKEN_2022,
            associated_token_program: ATA_PROGRAM,
            system_program: SYSTEM,
            damm_event_authority: damm_pda(&[b"__event_authority"]),
            damm_program: DAMM,
        }
        .to_account_metas(None),
    );
    send(&mut env.svm, &[compute_budget(1_400_000), ix], &[payer, &nft])?;
    Ok(k)
}

fn keys_of(env: &Env, mint: &Pubkey) -> PoolKeys {
    let c = curve_of(&env.svm, mint);
    let (quote, _) = quote_of(&env.svm, mint);
    PoolKeys {
        pool: c.pool,
        position: c.position,
        nft_account: c.position_nft_account,
        base_vault: damm_pda(&[b"token_vault", mint.as_ref(), c.pool.as_ref()]),
        quote_vault: damm_pda(&[b"token_vault", quote.as_ref(), c.pool.as_ref()]),
    }
}

fn migrate(env: &mut Env, payer: &Keypair, mint: &Pubkey) -> Result<PoolKeys, String> {
    let (quote, qp) = quote_of(&env.svm, mint);
    let curve = pda(&[b"curve", mint.as_ref()]);
    let migrator = pda(&[b"migrator", mint.as_ref()]);
    let k = keys_of(env, mint);
    let ix = Instruction::new_with_bytes(
        artist_launchpad::id(),
        &artist_launchpad::instruction::Migrate {}.data(),
        artist_launchpad::accounts::Migrate {
            payer: payer.pubkey(),
            curve,
            migrator,
            base_mint: *mint,
            quote_mint: quote,
            base_vault: ata(&curve, mint, &TOKEN_2022),
            quote_vault: ata(&curve, &quote, &qp),
            migrator_base: ata(&migrator, mint, &TOKEN_2022),
            migrator_quote: ata(&migrator, &quote, &qp),
            position_nft_account: k.nft_account,
            pool_authority: damm_pda(&[b"pool_authority"]),
            pool: k.pool,
            position: k.position,
            pool_base_vault: k.base_vault,
            pool_quote_vault: k.quote_vault,
            base_token_program: TOKEN_2022,
            quote_token_program: qp,
            damm_event_authority: damm_pda(&[b"__event_authority"]),
            damm_program: DAMM,
        }
        .to_account_metas(None),
    );
    send(&mut env.svm, &[compute_budget(1_400_000), ix], &[payer])?;
    Ok(k)
}

fn claim_pool_ix(env: &Env, payer: &Pubkey, mint: &Pubkey, creator: &Pubkey) -> Instruction {
    let c = curve_of(&env.svm, mint);
    let (quote, qp) = quote_of(&env.svm, mint);
    let migrator = pda(&[b"migrator", mint.as_ref()]);
    let treasury = pda(&[b"treasury", c.artist_mint.as_ref()]);
    Instruction::new_with_bytes(
        artist_launchpad::id(),
        &artist_launchpad::instruction::ClaimPoolFees {}.data(),
        artist_launchpad::accounts::ClaimPoolFees {
            payer: *payer,
            config: pda(&[b"config"]),
            curve: pda(&[b"curve", mint.as_ref()]),
            artist: pda(&[b"artist", c.artist_mint.as_ref()]),
            treasury,
            migrator,
            base_mint: *mint,
            quote_mint: quote,
            migrator_base: ata(&migrator, mint, &TOKEN_2022),
            migrator_quote: ata(&migrator, &quote, &qp),
            platform_wallet: env.platform,
            platform_quote: ata(&env.platform, &quote, &qp),
            creator: *creator,
            creator_quote: ata(creator, &quote, &qp),
            treasury_quote: ata(&treasury, &quote, &qp),
            pool_authority: damm_pda(&[b"pool_authority"]),
            pool: c.pool,
            position: c.position,
            position_nft_account: c.position_nft_account,
            pool_base_vault: damm_pda(&[b"token_vault", mint.as_ref(), c.pool.as_ref()]),
            pool_quote_vault: damm_pda(&[b"token_vault", quote.as_ref(), c.pool.as_ref()]),
            base_token_program: TOKEN_2022,
            quote_token_program: qp,
            associated_token_program: ATA_PROGRAM,
            system_program: SYSTEM,
            damm_event_authority: damm_pda(&[b"__event_authority"]),
            damm_program: DAMM,
        }
        .to_account_metas(None),
    )
}

/// A trade on the graduated DAMM v2 pool: `buy` = quote in, token out.
fn pool_swap(env: &mut Env, trader: &Keypair, mint: &Pubkey, buy: bool, amount_in: u64) -> Result<(), String> {
    use artist_launchpad::damm_v2::{client, types::SwapParameters};
    let c = curve_of(&env.svm, mint);
    let (quote, qp) = quote_of(&env.svm, mint);
    let token = ata(&trader.pubkey(), mint, &TOKEN_2022);
    let quote_acc = ata(&trader.pubkey(), &quote, &qp);
    let (input, output) = if buy { (quote_acc, token) } else { (token, quote_acc) };
    let ix = Instruction::new_with_bytes(
        DAMM,
        &client::args::Swap { _params: SwapParameters { amount_in, minimum_amount_out: 0 } }.data(),
        client::accounts::Swap {
            pool_authority: damm_pda(&[b"pool_authority"]),
            pool: c.pool,
            input_token_account: input,
            output_token_account: output,
            token_a_vault: damm_pda(&[b"token_vault", mint.as_ref(), c.pool.as_ref()]),
            token_b_vault: damm_pda(&[b"token_vault", quote.as_ref(), c.pool.as_ref()]),
            token_a_mint: *mint,
            token_b_mint: quote,
            payer: trader.pubkey(),
            token_a_program: TOKEN_2022,
            token_b_program: qp,
            referral_token_account: None,
            event_authority: damm_pda(&[b"__event_authority"]),
            program: DAMM,
        }
        .to_account_metas(None),
    );
    let (out_mint, out_program) = if buy { (*mint, TOKEN_2022) } else { (quote, qp) };
    let open = spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(
        &trader.pubkey(), &trader.pubkey(), &out_mint, &out_program,
    );
    send(&mut env.svm, &[compute_budget(400_000), open, ix], &[trader])
}

fn position_of(env: &Env, key: &Pubkey) -> artist_launchpad::damm_v2::accounts::Position {
    let data = env.svm.get_account(key).unwrap().data;
    bytemuck::pod_read_unaligned::<artist_launchpad::damm_v2::accounts::Position>(&data[8..8 + std::mem::size_of::<artist_launchpad::damm_v2::accounts::Position>()])
}

fn pool_of(env: &Env, key: &Pubkey) -> artist_launchpad::damm_v2::accounts::Pool {
    let data = env.svm.get_account(key).unwrap().data;
    bytemuck::pod_read_unaligned::<artist_launchpad::damm_v2::accounts::Pool>(&data[8..8 + std::mem::size_of::<artist_launchpad::damm_v2::accounts::Pool>()])
}

#[test]
fn a_curve_must_be_sold_out_to_migrate() {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist(&mut env, &creator, "jo");
    let anyone = funded(&mut env.svm);
    assert!(migrate(&mut env, &anyone, &jo.mint).unwrap_err().contains("CurveNotComplete"));
}

#[test]
fn a_sold_out_artist_graduates_to_a_locked_damm_v2_pool() {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist(&mut env, &creator, "jo");
    let whale = funded(&mut env.svm);
    give_wsol(&mut env.svm, &whale.pubkey(), 500 * SOL);
    buy(&mut env, &whale, &jo.mint, 200 * SOL).unwrap();
    let raised = curve_of(&env.svm, &jo.mint).real_quote;

    let anyone = funded(&mut env.svm);
    let k = migrate(&mut env, &anyone, &jo.mint).unwrap();
    let c = curve_of(&env.svm, &jo.mint);
    assert!(c.migrated && c.pool == k.pool);
    assert_eq!(amount(&env.svm, &ata(&jo.curve, &jo.mint, &TOKEN_2022)), 0, "every remaining token went to the pool");
    let pool_tokens = amount(&env.svm, &k.base_vault);
    let pool_sol = amount(&env.svm, &k.quote_vault);
    assert!(pool_tokens > 206_000_000 * TOK && pool_tokens <= TOTAL_SUPPLY - ARTIST_CURVE.curve_supply, "pool holds the 20.7% reserve: {pool_tokens}");
    assert!((pool_sol as f64 / raised as f64 - 1.0).abs() < 1e-4, "pool holds the SOL raised, to 0.01% ({pool_sol} of {raised})");
    // The curve vault keeps exactly the fees not yet claimed.
    assert_eq!(amount(&env.svm, &ata(&jo.curve, &WSOL, &TOKEN)), c.fees_platform + c.fees_creator + c.fees_treasury);

    let position = position_of(&env, &k.position);
    assert_eq!(position.unlocked_liquidity, 0, "nothing withdrawable");
    assert!(position.permanent_locked_liquidity > 0, "locked forever");
    let pool = pool_of(&env, &k.pool);
    assert_eq!(pool.collect_fee_mode, 2, "x·y = k, fees in the quote");
    assert_eq!(pool.token_a_mint, jo.mint);
    // Graduation price ≈ the curve's last price.
    let curve_price = c.virtual_quote as f64 / c.virtual_base as f64;
    let pool_price = (pool.sqrt_price as f64 / 2f64.powi(64)).powi(2);
    assert!((pool_price / curve_price - 1.0).abs() < 0.0001, "pool {pool_price:e} vs curve {curve_price:e}");
    assert!(migrate(&mut env, &anyone, &jo.mint).unwrap_err().contains("AlreadyMigrated"));
}

#[test]
fn pool_fees_keep_flowing_30_40_30_after_graduation() {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist(&mut env, &creator, "jo");
    let whale = funded(&mut env.svm);
    give_wsol(&mut env.svm, &whale.pubkey(), 500 * SOL);
    buy(&mut env, &whale, &jo.mint, 200 * SOL).unwrap();
    let anyone = funded(&mut env.svm);
    migrate(&mut env, &anyone, &jo.mint).unwrap();
    // Claim the curve's own fees first, so the pool fees can be told apart.
    { let ix = claim_ix(&env, &anyone.pubkey(), &jo.mint, &creator.pubkey()); send(&mut env.svm, &[ix], &[&anyone]).unwrap(); }
    let treasury = pda(&[b"treasury", jo.mint.as_ref()]);
    let before = (
        amount(&env.svm, &ata(&env.platform, &WSOL, &TOKEN)),
        amount(&env.svm, &ata(&creator.pubkey(), &WSOL, &TOKEN)),
        amount(&env.svm, &ata(&treasury, &WSOL, &TOKEN)),
    );

    // Trading on the DAMM v2 pool: buys in SOL, a sell.
    let fan = funded(&mut env.svm);
    give_wsol(&mut env.svm, &fan.pubkey(), 50 * SOL);
    pool_swap(&mut env, &fan, &jo.mint, true, 20 * SOL).unwrap();
    let got = amount(&env.svm, &ata(&fan.pubkey(), &jo.mint, &TOKEN_2022));
    assert!(got > 0, "bought $JO on the pool");
    pool_swap(&mut env, &fan, &jo.mint, false, got / 2).unwrap();

    { let ix = claim_pool_ix(&env, &anyone.pubkey(), &jo.mint, &creator.pubkey()); send(&mut env.svm, &[ix], &[&anyone]).unwrap(); }
    let after = (
        amount(&env.svm, &ata(&env.platform, &WSOL, &TOKEN)),
        amount(&env.svm, &ata(&creator.pubkey(), &WSOL, &TOKEN)),
        amount(&env.svm, &ata(&treasury, &WSOL, &TOKEN)),
    );
    let (p, c, t) = (after.0 - before.0, after.1 - before.1, after.2 - before.2);
    let total = p + c + t;
    println!("pool fees claimed: {} SOL (platform {}, creator {}, treasury {})", total as f64 / 1e9, p, c, t);
    assert!(total > 100_000_000, "1% of ~30 SOL traded, minus Meteora's protocol share, reaches us");
    assert_eq!(p, total * 3 / 10);
    assert_eq!(c, total * 4 / 10);
    assert_eq!(t, total - total * 3 / 10 - total * 4 / 10);
}

#[test]
fn a_sold_out_song_graduates_to_a_song_artist_pool() {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist(&mut env, &creator, "jo");
    let a = launch_song(&mut env, &creator, &jo.mint, 0, "a");
    let whale = funded(&mut env.svm);
    give_wsol(&mut env.svm, &whale.pubkey(), 500 * SOL);
    buy(&mut env, &whale, &jo.mint, 60 * SOL).unwrap(); // $JO to buy the song with
    let jo_bal = amount(&env.svm, &ata(&whale.pubkey(), &jo.mint, &TOKEN_2022));
    buy(&mut env, &whale, &a.mint, jo_bal).unwrap();
    assert!(curve_of(&env.svm, &a.mint).complete, "song curve sold out in $JO");
    let anyone = funded(&mut env.svm);
    let k = migrate(&mut env, &anyone, &a.mint).unwrap();
    let pool = pool_of(&env, &k.pool);
    assert_eq!((pool.token_a_mint, pool.token_b_mint), (a.mint, jo.mint), "a $A/$JO pool");
    assert!(amount(&env.svm, &k.quote_vault) > 80_000_000 * TOK, "holding the $JO raised");

    // Trading $A against $JO on the pool, fees in $JO split 30/40/30.
    let fan = funded(&mut env.svm);
    give_wsol(&mut env.svm, &fan.pubkey(), 20 * SOL);
    buy(&mut env, &fan, &jo.mint, 10 * SOL).unwrap();
    let fan_jo = amount(&env.svm, &ata(&fan.pubkey(), &jo.mint, &TOKEN_2022));
    pool_swap(&mut env, &fan, &a.mint, true, fan_jo / 2).unwrap();
    let supply_before = jo_supply(&env.svm, &jo.mint);
    let seed_fund = amount(&env.svm, &ata(&pda(&[b"treasury", jo.mint.as_ref()]), &jo.mint, &TOKEN_2022));
    { let ix = claim_pool_ix(&env, &anyone.pubkey(), &a.mint, &creator.pubkey()); send(&mut env.svm, &[ix], &[&anyone]).unwrap(); }
    let treasury = pda(&[b"treasury", jo.mint.as_ref()]);
    let burned = supply_before - jo_supply(&env.svm, &jo.mint);
    let to_creator = amount(&env.svm, &ata(&creator.pubkey(), &jo.mint, &TOKEN_2022));
    assert_eq!(amount(&env.svm, &ata(&treasury, &jo.mint, &TOKEN_2022)), seed_fund, "a song's pool fees never reach the treasury");
    assert!(burned > 0 && to_creator > 0, "the creator earns $JO from the song's pool, the treasury share is burned");
    assert!((burned as i128 - (to_creator as i128 * 3 / 4)).abs() <= 2, "burn 30% vs creator 40%: {burned} vs {to_creator}");
}

// ------------------------------------------------------------------------------------------------ the armed pool

fn sqrt_price_of(env: &Env, pool: &Pubkey) -> f64 {
    (pool_of(env, pool).sqrt_price as f64 / 2f64.powi(64)).powi(2)
}

#[test]
fn trading_opens_once_the_pool_is_armed() {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist_unarmed(&mut env, &creator, "jo");
    let fan = funded(&mut env.svm);
    give_wsol(&mut env.svm, &fan.pubkey(), 10 * SOL);
    assert!(buy(&mut env, &fan, &jo.mint, SOL).unwrap_err().contains("PoolNotArmed"), "nobody holds a token before the pool exists");
    let payer = funded(&mut env.svm);
    let k = arm(&mut env, &payer, &jo.mint).unwrap();
    let pool = pool_of(&env, &k.pool);
    assert_eq!((pool.token_a_mint, pool.token_b_mint), (jo.mint, WSOL));
    let position = position_of(&env, &k.position);
    assert_eq!(position.unlocked_liquidity, 0, "the dust position is locked too");
    // Armed at the price the curve will graduate at.
    let c = curve_of(&env.svm, &jo.mint);
    let raise = ARTIST_CURVE.virtual_quote as f64 * ARTIST_CURVE.virtual_base as f64 / (ARTIST_CURVE.virtual_base - ARTIST_CURVE.curve_supply) as f64 - ARTIST_CURVE.virtual_quote as f64;
    let expected = raise / (TOTAL_SUPPLY - ARTIST_CURVE.curve_supply) as f64;
    assert!((sqrt_price_of(&env, &k.pool) / expected - 1.0).abs() < 1e-5, "armed at the graduation price: {:e} vs {expected:e}", sqrt_price_of(&env, &k.pool));
    let treasury = pda(&[b"treasury", jo.mint.as_ref()]);
    assert_eq!(amount(&env.svm, &ata(&treasury, &jo.mint, &TOKEN_2022)), artist_launchpad::SONG_SEED_FUND, "the treasury keeps $JO to arm its songs' pools");
    assert_eq!(c.real_base, ARTIST_CURVE.curve_supply, "the dust comes from the reserve, not from the curve");
    buy(&mut env, &fan, &jo.mint, SOL).unwrap();
    assert!(arm(&mut env, &payer, &jo.mint).unwrap_err().contains("AlreadyArmed"));
}

#[test]
fn nobody_can_take_the_pool_address_first() {
    use artist_launchpad::damm_v2::{client, types::{BaseFeeParameters, InitializeCustomizablePoolParameters, PoolFeeParameters}};
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist(&mut env, &creator, "jo");
    // A griefer with both tokens tries to open the pair's pool.
    let griefer = funded(&mut env.svm);
    give_wsol(&mut env.svm, &griefer.pubkey(), 10 * SOL);
    buy(&mut env, &griefer, &jo.mint, SOL).unwrap();
    let nft = Keypair::new();
    let k = pool_keys(&jo.mint, &WSOL, &nft.pubkey());
    let mut fee = [0u8; 27];
    fee[..8].copy_from_slice(&10_000_000u64.to_le_bytes());
    let ix = Instruction::new_with_bytes(
        DAMM,
        &client::args::InitializeCustomizablePool {
            params: InitializeCustomizablePoolParameters {
                pool_fees: PoolFeeParameters { base_fee: BaseFeeParameters { data: fee }, compounding_fee_bps: 0, padding: 0, dynamic_fee: None },
                sqrt_min_price: 4_295_048_016,
                sqrt_max_price: 79_226_673_521_066_979_257_578_248_091,
                has_alpha_vault: false,
                liquidity: 200u128 << 64,
                sqrt_price: 1u128 << 60,
                activation_type: 1,
                collect_fee_mode: 0,
                activation_point: None,
            },
        }
        .data(),
        client::accounts::InitializeCustomizablePool {
            creator: griefer.pubkey(),
            position_nft_mint: nft.pubkey(),
            position_nft_account: k.nft_account,
            payer: griefer.pubkey(),
            pool_authority: damm_pda(&[b"pool_authority"]),
            pool: k.pool,
            position: k.position,
            token_a_mint: jo.mint,
            token_b_mint: WSOL,
            token_a_vault: k.base_vault,
            token_b_vault: k.quote_vault,
            payer_token_a: ata(&griefer.pubkey(), &jo.mint, &TOKEN_2022),
            payer_token_b: ata(&griefer.pubkey(), &WSOL, &TOKEN),
            token_a_program: TOKEN_2022,
            token_b_program: TOKEN,
            token_2022_program: TOKEN_2022,
            system_program: SYSTEM,
            event_authority: damm_pda(&[b"__event_authority"]),
            program: DAMM,
        }
        .to_account_metas(None),
    );
    assert!(send(&mut env.svm, &[compute_budget(1_400_000), ix], &[&griefer, &nft]).is_err(), "the pair's pool address is already ours");
    assert_eq!(k.pool, curve_of(&env.svm, &jo.mint).pool);
}

/// Someone moves the dust pool's price before graduation (up or down): migrate trades it back to the curve's final
/// price before adding the liquidity.
fn moved_pool_graduates_at_the_curve_price(push_up: bool) {
    let mut env = setup();
    let creator = funded(&mut env.svm);
    let jo = launch_artist(&mut env, &creator, "jo");
    let k = keys_of(&env, &jo.mint);
    let griefer = funded(&mut env.svm);
    give_wsol(&mut env.svm, &griefer.pubkey(), 20 * SOL);
    buy(&mut env, &griefer, &jo.mint, 2 * SOL).unwrap();
    let armed = sqrt_price_of(&env, &k.pool);
    if push_up {
        pool_swap(&mut env, &griefer, &jo.mint, true, SOL).unwrap(); // buys the dust tokens: price flies
    } else {
        let held = amount(&env.svm, &ata(&griefer.pubkey(), &jo.mint, &TOKEN_2022));
        pool_swap(&mut env, &griefer, &jo.mint, false, held / 2).unwrap(); // dumps tokens: price sinks
    }
    let moved = sqrt_price_of(&env, &k.pool);
    assert!(if push_up { moved > armed * 100.0 } else { moved < armed / 100.0 }, "the dust pool was moved: {armed:e} -> {moved:e}");

    let whale = funded(&mut env.svm);
    give_wsol(&mut env.svm, &whale.pubkey(), 500 * SOL);
    buy(&mut env, &whale, &jo.mint, 200 * SOL).unwrap();
    let raised = curve_of(&env.svm, &jo.mint).real_quote;
    let anyone = funded(&mut env.svm);
    migrate(&mut env, &anyone, &jo.mint).unwrap();
    let c = curve_of(&env.svm, &jo.mint);
    let curve_price = c.virtual_quote as f64 / c.virtual_base as f64;
    let pool_price = sqrt_price_of(&env, &k.pool);
    assert!((pool_price / curve_price - 1.0).abs() < 0.002, "graduated at the curve price: pool {pool_price:e} vs curve {curve_price:e}");
    let pool_sol = amount(&env.svm, &k.quote_vault);
    assert!((pool_sol as f64 / raised as f64 - 1.0).abs() < 0.01, "the pool holds the SOL raised: {pool_sol} of {raised}");
    let position = position_of(&env, &k.position);
    assert_eq!(position.unlocked_liquidity, 0, "everything locked");
    assert!(position.permanent_locked_liquidity > 1u128 << 80, "the graduation liquidity is in the program's position");
}

#[test]
fn a_pool_pushed_up_is_traded_back_at_graduation() {
    moved_pool_graduates_at_the_curve_price(true);
}

#[test]
fn a_pool_pushed_down_is_traded_back_at_graduation() {
    moved_pool_graduates_at_the_curve_price(false);
}

