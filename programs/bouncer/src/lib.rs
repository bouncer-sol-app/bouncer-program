//! Bouncer launchpad program.
//!
//! Door policy:
//! - Only wallets with a `Permit` (issued by the gatekeeper after an X login) can buy or create coins.
//! - Each wallet can hold at most 3% of a coin's supply from the curve: its buys minus its own sells.
//!   Selling back to the curve from that same wallet frees the limit again; transferring tokens away does not.
//! - Coins trade on a constant-product bonding curve until the curve has raised
//!   `config.migration_lamports`. Then trading stops and the migrator moves liquidity to a DEX.

use anchor_lang::prelude::*;
use anchor_lang::system_program;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_2022::spl_token_2022::instruction::AuthorityType;
use anchor_spl::token_2022::{self, MintTo, SetAuthority, Token2022};
use anchor_spl::token_2022_extensions::{token_metadata_initialize, TokenMetadataInitialize};
use anchor_spl::token_interface::{transfer_checked, Mint, TokenAccount, TransferChecked};

pub mod math;
use math::*;

declare_id!("BNCo74BKMR2jg4FLcvKpwbXJdABMzvYMzNwwaqefqnmu");

#[program]
pub mod bouncer {
    use super::*;

    /// One-time setup. Only the program's upgrade authority may call it.
    pub fn initialize(ctx: Context<Initialize>, params: ConfigParams) -> Result<()> {
        params.validate()?;
        let config = &mut ctx.accounts.config;
        config.admin = ctx.accounts.admin.key();
        config.gatekeeper = params.gatekeeper;
        config.migrator = params.migrator;
        config.fee_recipient = params.fee_recipient;
        config.fee_bps = params.fee_bps;
        config.migration_lamports = params.migration_lamports;
        config.paused = false;
        config.bump = ctx.bumps.config;
        Ok(())
    }

    pub fn update_config(ctx: Context<UpdateConfig>, params: ConfigParams, new_admin: Pubkey, paused: bool) -> Result<()> {
        params.validate()?;
        let config = &mut ctx.accounts.config;
        config.admin = new_admin;
        config.gatekeeper = params.gatekeeper;
        config.migrator = params.migrator;
        config.fee_recipient = params.fee_recipient;
        config.fee_bps = params.fee_bps;
        config.migration_lamports = params.migration_lamports;
        config.paused = paused;
        Ok(())
    }

    /// Gatekeeper lets a wallet in (called by the backend after X login + wallet link).
    pub fn register_wallet(ctx: Context<RegisterWallet>, wallet: Pubkey) -> Result<()> {
        let permit = &mut ctx.accounts.permit;
        permit.wallet = wallet;
        permit.bump = ctx.bumps.permit;
        emit!(WalletRegistered { wallet });
        Ok(())
    }

    /// Gatekeeper removes a wallet's permit.
    pub fn revoke_wallet(_ctx: Context<RevokeWallet>, _wallet: Pubkey) -> Result<()> {
        Ok(())
    }

    /// Creates a coin: Token-2022 mint with on-mint metadata, full supply minted to the curve,
    /// then the mint authority is removed for good.
    pub fn create_token(ctx: Context<CreateToken>, name: String, symbol: String, uri: String) -> Result<()> {
        require!(!ctx.accounts.config.paused, BouncerError::Paused);
        require!(!name.is_empty() && name.len() <= 32, BouncerError::InvalidMetadata);
        require!(!symbol.is_empty() && symbol.len() <= 10, BouncerError::InvalidMetadata);
        require!(uri.len() <= 200, BouncerError::InvalidMetadata);

        let mint_key = ctx.accounts.mint.key();
        let curve_bump = ctx.bumps.curve;
        let seeds: &[&[u8]] = &[CURVE_SEED, mint_key.as_ref(), &[curve_bump]];
        let signer = &[seeds];

        // Top up the mint so it can hold the metadata TLV entry.
        let extra = metadata_space(&name, &symbol, &uri);
        let mint_info = ctx.accounts.mint.to_account_info();
        let needed = Rent::get()?.minimum_balance(mint_info.data_len() + extra);
        let have = mint_info.lamports();
        if needed > have {
            system_program::transfer(
                CpiContext::new(
                    ctx.accounts.system_program.key(),
                    system_program::Transfer {
                        from: ctx.accounts.creator.to_account_info(),
                        to: mint_info.clone(),
                    },
                ),
                needed - have,
            )?;
        }

        token_metadata_initialize(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                TokenMetadataInitialize {
                    program_id: ctx.accounts.token_program.to_account_info(),
                    metadata: mint_info.clone(),
                    update_authority: ctx.accounts.curve.to_account_info(),
                    mint_authority: ctx.accounts.curve.to_account_info(),
                    mint: mint_info.clone(),
                },
                signer,
            ),
            name.clone(),
            symbol.clone(),
            uri.clone(),
        )?;

        token_2022::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                MintTo {
                    mint: mint_info.clone(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.curve.to_account_info(),
                },
                signer,
            ),
            TOTAL_SUPPLY,
        )?;

        token_2022::set_authority(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                SetAuthority {
                    current_authority: ctx.accounts.curve.to_account_info(),
                    account_or_mint: mint_info,
                },
                signer,
            ),
            AuthorityType::MintTokens,
            None,
        )?;

        let curve = &mut ctx.accounts.curve;
        curve.mint = mint_key;
        curve.creator = ctx.accounts.creator.key();
        curve.virtual_sol_reserves = INITIAL_VIRTUAL_SOL;
        curve.virtual_token_reserves = INITIAL_VIRTUAL_TOKENS;
        curve.real_sol_reserves = 0;
        curve.real_token_reserves = CURVE_SELLABLE_TOKENS;
        curve.complete = false;
        curve.migrated = false;
        curve.bump = curve_bump;

        emit!(TokenCreated { mint: mint_key, creator: curve.creator, name, symbol, uri });
        Ok(())
    }

    /// Buy with at most `max_sol_in` lamports (fee included). Fails if the wallet would pass 3%.
    pub fn buy(ctx: Context<Buy>, max_sol_in: u64, min_tokens_out: u64) -> Result<()> {
        let config = &ctx.accounts.config;
        require!(!config.paused, BouncerError::Paused);
        let curve = &mut ctx.accounts.curve;
        require!(!curve.complete, BouncerError::CurveComplete);

        let quote = quote_buy(
            curve.virtual_sol_reserves,
            curve.virtual_token_reserves,
            curve.real_token_reserves,
            max_sol_in,
            config.fee_bps,
        )
        .ok_or(BouncerError::MathOverflow)?;
        require!(quote.tokens_out > 0, BouncerError::ZeroAmount);
        require!(quote.tokens_out >= min_tokens_out, BouncerError::SlippageExceeded);

        // The 3% cap applies to what this wallet holds from the curve: its buys minus its own sells.
        let position = &mut ctx.accounts.position;
        let holding_after = position.holding.checked_add(quote.tokens_out).ok_or(BouncerError::MathOverflow)?;
        require!(holding_after <= MAX_BUY_PER_WALLET, BouncerError::WalletCapExceeded);

        // SOL: net amount into the curve, fee to the fee recipient.
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                system_program::Transfer {
                    from: ctx.accounts.buyer.to_account_info(),
                    to: curve.to_account_info(),
                },
            ),
            quote.sol_net,
        )?;
        if quote.fee > 0 {
            system_program::transfer(
                CpiContext::new(
                    ctx.accounts.system_program.key(),
                    system_program::Transfer {
                        from: ctx.accounts.buyer.to_account_info(),
                        to: ctx.accounts.fee_recipient.to_account_info(),
                    },
                ),
                quote.fee,
            )?;
        }

        // Tokens: vault -> buyer.
        let mint_key = ctx.accounts.mint.key();
        let seeds: &[&[u8]] = &[CURVE_SEED, mint_key.as_ref(), &[curve.bump]];
        transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.vault.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.buyer_token_account.to_account_info(),
                    authority: curve.to_account_info(),
                },
                &[seeds],
            ),
            quote.tokens_out,
            DECIMALS,
        )?;

        curve.virtual_sol_reserves += quote.sol_net;
        curve.virtual_token_reserves -= quote.tokens_out;
        curve.real_sol_reserves += quote.sol_net;
        curve.real_token_reserves -= quote.tokens_out;
        position.wallet = ctx.accounts.buyer.key();
        position.mint = mint_key;
        position.holding = holding_after;
        position.bought = position.bought.checked_add(quote.tokens_out).ok_or(BouncerError::MathOverflow)?;
        position.bump = ctx.bumps.position;

        emit!(Trade {
            mint: mint_key,
            wallet: position.wallet,
            is_buy: true,
            sol_amount: quote.sol_net,
            token_amount: quote.tokens_out,
            fee: quote.fee,
            virtual_sol_reserves: curve.virtual_sol_reserves,
            virtual_token_reserves: curve.virtual_token_reserves,
        });

        if curve.real_sol_reserves >= config.migration_lamports || curve.real_token_reserves == 0 {
            curve.complete = true;
            emit!(CurveCompleted { mint: mint_key, real_sol_reserves: curve.real_sol_reserves });
        }
        Ok(())
    }

    /// Sell tokens back to the curve. Anyone holding the token can sell.
    /// Only sells from the buying wallet itself free up that wallet's 3% limit;
    /// tokens sent elsewhere and sold there keep counting against it.
    pub fn sell(ctx: Context<Sell>, token_amount: u64, min_sol_out: u64) -> Result<()> {
        let config = &ctx.accounts.config;
        require!(!config.paused, BouncerError::Paused);
        let curve = &mut ctx.accounts.curve;
        require!(!curve.complete, BouncerError::CurveComplete);
        require!(token_amount > 0, BouncerError::ZeroAmount);

        let quote = quote_sell(curve.virtual_sol_reserves, curve.virtual_token_reserves, token_amount, config.fee_bps)
            .ok_or(BouncerError::MathOverflow)?;
        require!(quote.sol_gross <= curve.real_sol_reserves, BouncerError::InsufficientLiquidity);
        require!(quote.sol_net >= min_sol_out, BouncerError::SlippageExceeded);

        transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.seller_token_account.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.seller.to_account_info(),
                },
            ),
            token_amount,
            DECIMALS,
        )?;

        // The curve is owned by this program, so lamports can be moved directly.
        let curve_info = curve.to_account_info();
        **curve_info.try_borrow_mut_lamports()? -= quote.sol_gross;
        **ctx.accounts.seller.to_account_info().try_borrow_mut_lamports()? += quote.sol_net;
        **ctx.accounts.fee_recipient.to_account_info().try_borrow_mut_lamports()? += quote.fee;

        if let Some(position) = ctx.accounts.position.as_mut() {
            position.holding = position.holding.saturating_sub(token_amount);
        }

        curve.virtual_sol_reserves -= quote.sol_gross;
        curve.virtual_token_reserves += token_amount;
        curve.real_sol_reserves -= quote.sol_gross;
        curve.real_token_reserves += token_amount;

        emit!(Trade {
            mint: ctx.accounts.mint.key(),
            wallet: ctx.accounts.seller.key(),
            is_buy: false,
            sol_amount: quote.sol_net,
            token_amount,
            fee: quote.fee,
            virtual_sol_reserves: curve.virtual_sol_reserves,
            virtual_token_reserves: curve.virtual_token_reserves,
        });
        Ok(())
    }

    /// After the curve completes, the migrator (or admin) takes the SOL and all remaining tokens
    /// to seed the DEX pool.
    pub fn withdraw_for_migration(ctx: Context<WithdrawForMigration>) -> Result<()> {
        let curve = &mut ctx.accounts.curve;
        require!(curve.complete, BouncerError::CurveNotComplete);
        require!(!curve.migrated, BouncerError::AlreadyMigrated);

        let sol = curve.real_sol_reserves;
        let curve_info = curve.to_account_info();

        // CPI first; direct lamport moves must come after any CPI in the same instruction.
        let mint_key = ctx.accounts.mint.key();
        let seeds: &[&[u8]] = &[CURVE_SEED, mint_key.as_ref(), &[curve.bump]];
        let tokens = ctx.accounts.vault.amount;
        transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.vault.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.migrator_token_account.to_account_info(),
                    authority: curve_info.clone(),
                },
                &[seeds],
            ),
            tokens,
            DECIMALS,
        )?;

        **curve_info.try_borrow_mut_lamports()? -= sol;
        **ctx.accounts.migrator.to_account_info().try_borrow_mut_lamports()? += sol;

        curve.real_sol_reserves = 0;
        curve.real_token_reserves = 0;
        curve.migrated = true;
        emit!(Migrated { mint: mint_key, sol_amount: sol, token_amount: tokens });
        Ok(())
    }
}

// ---------------------------------------------------------------- seeds

pub const CONFIG_SEED: &[u8] = b"config";
pub const PERMIT_SEED: &[u8] = b"permit";
pub const CURVE_SEED: &[u8] = b"curve";
pub const POSITION_SEED: &[u8] = b"position";

// ---------------------------------------------------------------- state

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    pub gatekeeper: Pubkey,
    /// Server key that may only move finished curves to the DEX.
    pub migrator: Pubkey,
    pub fee_recipient: Pubkey,
    pub fee_bps: u16,
    pub migration_lamports: u64,
    pub paused: bool,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Permit {
    pub wallet: Pubkey,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Curve {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub complete: bool,
    pub migrated: bool,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Position {
    pub wallet: Pubkey,
    pub mint: Pubkey,
    /// Tokens bought from the curve minus tokens this same wallet sold back. Capped at 3%.
    /// Transfers to other wallets do not lower it.
    pub holding: u64,
    /// Lifetime total bought from the curve (for stats).
    pub bought: u64,
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct ConfigParams {
    pub gatekeeper: Pubkey,
    pub migrator: Pubkey,
    pub fee_recipient: Pubkey,
    pub fee_bps: u16,
    pub migration_lamports: u64,
}

impl ConfigParams {
    fn validate(&self) -> Result<()> {
        require!(self.fee_bps <= MAX_FEE_BPS, BouncerError::FeeTooHigh);
        require!(self.migration_lamports > 0, BouncerError::ZeroAmount);
        Ok(())
    }
}

// ---------------------------------------------------------------- accounts

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + Config::INIT_SPACE, seeds = [CONFIG_SEED], bump)]
    pub config: Account<'info, Config>,
    #[account(constraint = program.programdata_address()? == Some(program_data.key()) @ BouncerError::Unauthorized)]
    pub program: Program<'info, crate::program::Bouncer>,
    #[account(constraint = program_data.upgrade_authority_address == Some(admin.key()) @ BouncerError::Unauthorized)]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ BouncerError::Unauthorized)]
    pub config: Account<'info, Config>,
}

#[derive(Accounts)]
#[instruction(wallet: Pubkey)]
pub struct RegisterWallet<'info> {
    #[account(mut)]
    pub gatekeeper: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = gatekeeper @ BouncerError::Unauthorized)]
    pub config: Account<'info, Config>,
    #[account(init, payer = gatekeeper, space = 8 + Permit::INIT_SPACE, seeds = [PERMIT_SEED, wallet.as_ref()], bump)]
    pub permit: Account<'info, Permit>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(wallet: Pubkey)]
pub struct RevokeWallet<'info> {
    #[account(mut)]
    pub gatekeeper: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = gatekeeper @ BouncerError::Unauthorized)]
    pub config: Account<'info, Config>,
    #[account(mut, close = gatekeeper, seeds = [PERMIT_SEED, wallet.as_ref()], bump = permit.bump)]
    pub permit: Account<'info, Permit>,
}

#[derive(Accounts)]
pub struct CreateToken<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(seeds = [PERMIT_SEED, creator.key().as_ref()], bump = permit.bump)]
    pub permit: Account<'info, Permit>,
    #[account(
        init,
        payer = creator,
        mint::decimals = DECIMALS,
        mint::authority = curve,
        mint::token_program = token_program,
        extensions::metadata_pointer::authority = curve,
        extensions::metadata_pointer::metadata_address = mint,
    )]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(init, payer = creator, space = 8 + Curve::INIT_SPACE, seeds = [CURVE_SEED, mint.key().as_ref()], bump)]
    pub curve: Account<'info, Curve>,
    #[account(
        init,
        payer = creator,
        associated_token::mint = mint,
        associated_token::authority = curve,
        associated_token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Program<'info, Token2022>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Buy<'info> {
    #[account(mut)]
    pub buyer: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = fee_recipient)]
    pub config: Account<'info, Config>,
    /// CHECK: checked against config.fee_recipient
    #[account(mut)]
    pub fee_recipient: UncheckedAccount<'info>,
    #[account(seeds = [PERMIT_SEED, buyer.key().as_ref()], bump = permit.bump)]
    pub permit: Account<'info, Permit>,
    #[account(mut, seeds = [CURVE_SEED, mint.key().as_ref()], bump = curve.bump, has_one = mint)]
    pub curve: Account<'info, Curve>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, associated_token::mint = mint, associated_token::authority = curve, associated_token::token_program = token_program)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init_if_needed,
        payer = buyer,
        associated_token::mint = mint,
        associated_token::authority = buyer,
        associated_token::token_program = token_program,
    )]
    pub buyer_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init_if_needed,
        payer = buyer,
        space = 8 + Position::INIT_SPACE,
        seeds = [POSITION_SEED, mint.key().as_ref(), buyer.key().as_ref()],
        bump,
    )]
    pub position: Account<'info, Position>,
    pub token_program: Program<'info, Token2022>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Sell<'info> {
    #[account(mut)]
    pub seller: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = fee_recipient)]
    pub config: Account<'info, Config>,
    /// CHECK: checked against config.fee_recipient
    #[account(mut)]
    pub fee_recipient: UncheckedAccount<'info>,
    #[account(mut, seeds = [CURVE_SEED, mint.key().as_ref()], bump = curve.bump, has_one = mint)]
    pub curve: Account<'info, Curve>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, associated_token::mint = mint, associated_token::authority = curve, associated_token::token_program = token_program)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = mint, token::authority = seller, token::token_program = token_program)]
    pub seller_token_account: InterfaceAccount<'info, TokenAccount>,
    /// The seller's own position, if the seller ever bought this coin from the curve.
    #[account(mut, seeds = [POSITION_SEED, mint.key().as_ref(), seller.key().as_ref()], bump = position.bump)]
    pub position: Option<Account<'info, Position>>,
    pub token_program: Program<'info, Token2022>,
}

#[derive(Accounts)]
pub struct WithdrawForMigration<'info> {
    #[account(mut, constraint = migrator.key() == config.migrator || migrator.key() == config.admin @ BouncerError::Unauthorized)]
    pub migrator: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [CURVE_SEED, mint.key().as_ref()], bump = curve.bump, has_one = mint)]
    pub curve: Account<'info, Curve>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, associated_token::mint = mint, associated_token::authority = curve, associated_token::token_program = token_program)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init_if_needed,
        payer = migrator,
        associated_token::mint = mint,
        associated_token::authority = migrator,
        associated_token::token_program = token_program,
    )]
    pub migrator_token_account: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Program<'info, Token2022>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

// ---------------------------------------------------------------- events

#[event]
pub struct WalletRegistered {
    pub wallet: Pubkey,
}

#[event]
pub struct TokenCreated {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub name: String,
    pub symbol: String,
    pub uri: String,
}

#[event]
pub struct Trade {
    pub mint: Pubkey,
    pub wallet: Pubkey,
    pub is_buy: bool,
    pub sol_amount: u64,
    pub token_amount: u64,
    pub fee: u64,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
}

#[event]
pub struct CurveCompleted {
    pub mint: Pubkey,
    pub real_sol_reserves: u64,
}

#[event]
pub struct Migrated {
    pub mint: Pubkey,
    pub sol_amount: u64,
    pub token_amount: u64,
}

// ---------------------------------------------------------------- errors

#[error_code]
pub enum BouncerError {
    #[msg("Not allowed")]
    Unauthorized,
    #[msg("Trading is paused")]
    Paused,
    #[msg("This wallet would go over 3% of the supply")]
    WalletCapExceeded,
    #[msg("Price moved more than your slippage")]
    SlippageExceeded,
    #[msg("The bonding curve is complete")]
    CurveComplete,
    #[msg("The bonding curve is not complete yet")]
    CurveNotComplete,
    #[msg("Already migrated")]
    AlreadyMigrated,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Not enough SOL in the curve")]
    InsufficientLiquidity,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Fee is too high")]
    FeeTooHigh,
    #[msg("Invalid name, symbol or URI")]
    InvalidMetadata,
}
