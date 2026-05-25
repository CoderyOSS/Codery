#!/bin/bash
set -euo pipefail

ARCH=$(uname -m)
case "$ARCH" in
  x86_64)  NVIM_ARCH="linux-x86_64" ;;
  aarch64) NVIM_ARCH="linux-arm64" ;;
  *) echo "Unsupported arch: $ARCH"; exit 1 ;;
esac

curl -fsSL "https://github.com/neovim/neovim/releases/latest/download/nvim-${NVIM_ARCH}.tar.gz" \
  | tar xzf - -C /usr/local --strip-components=1

npm install -g typescript typescript-language-server prettier
rustup component add rust-analyzer
pip install --break-system-packages basedpyright ruff

mkdir -p /home/gem/.config/nvim

cat > /home/gem/.config/nvim/init.lua << 'NVIMEOF'
vim.g.mapleader = " "
vim.g.maplocalleader = " "

vim.opt.mouse = "a"
vim.opt.number = true
vim.opt.relativenumber = false
vim.opt.expandtab = true
vim.opt.shiftwidth = 2
vim.opt.tabstop = 2
vim.opt.softtabstop = 2
vim.opt.undofile = true
vim.opt.ignorecase = true
vim.opt.smartcase = true
vim.opt.signcolumn = "yes"
vim.opt.updatetime = 250
vim.opt.timeoutlen = 300
vim.opt.termguicolors = true

local lazypath = vim.fn.stdpath("data") .. "/lazy/lazy.nvim"
if not (vim.uv or vim.loop).fs_stat(lazypath) then
  vim.fn.system({
    "git", "clone", "--filter=blob:none",
    "https://github.com/folke/lazy.nvim.git",
    "--branch=stable", lazypath,
  })
end
vim.opt.rtp:prepend(lazypath)

require("lazy").setup({
  { "catppuccin/nvim", name = "catppuccin", priority = 1000 },

  { "neovim/nvim-lspconfig" },

  {
    "saghen/blink.cmp",
    version = "*",
    opts = {
      keymap = { preset = "default" },
      sources = {
        default = { "lsp", "path", "buffer" },
        providers = {
          buffer = { min_keyword_length = 3 },
        },
      },
    },
  },

  {
    "nvim-treesitter/nvim-treesitter",
    build = ":TSUpdate",
    config = function()
      require("nvim-treesitter.configs").setup({
        ensure_installed = {
          "typescript", "tsx", "rust", "python",
          "json", "yaml", "toml", "bash", "markdown", "lua",
        },
        auto_install = true,
        highlight = { enable = true },
        indent = { enable = true },
      })
    end,
  },

  {
    "nvim-neo-tree/neo-tree.nvim",
    branch = "v3.x",
    dependencies = {
      "nvim-lua/plenary.nvim",
      "nvim-tree/nvim-web-devicons",
      "MunifTanjim/nui.nvim",
    },
    opts = {
      filesystem = {
        filtered_items = {
          hide_dotfiles = false,
          hide_gitignored = true,
        },
      },
      window = { width = 35 },
    },
    keys = {
      { "<leader>e", "<cmd>Neotree toggle<cr>", desc = "Toggle file explorer" },
    },
  },

  {
    "nvim-telescope/telescope.nvim",
    branch = "0.1.x",
    dependencies = { "nvim-lua/plenary.nvim" },
    keys = {
      { "<leader>ff", "<cmd>Telescope find_files<cr>",            desc = "Find files" },
      { "<leader>fg", "<cmd>Telescope live_grep<cr>",             desc = "Live grep" },
      { "<leader>fb", "<cmd>Telescope buffers<cr>",               desc = "Buffers" },
      { "<leader>fh", "<cmd>Telescope help_tags<cr>",             desc = "Help tags" },
      { "<leader>fd", "<cmd>Telescope diagnostics<cr>",           desc = "Diagnostics" },
      { "<leader>fs", "<cmd>Telescope lsp_document_symbols<cr>",  desc = "Document symbols" },
    },
    opts = {
      defaults = {
        layout_strategy = "horizontal",
        layout_config = { prompt_position = "top" },
        sorting_strategy = "ascending",
      },
    },
  },

  {
    "lewis6991/gitsigns.nvim",
    opts = {
      signs = {
        add          = { text = "▎" },
        change       = { text = "▎" },
        delete       = { text = "▎" },
        topdelete    = { text = "▎" },
        changedelete = { text = "▎" },
      },
      on_attach = function(bufnr)
        local gs = package.loaded.gitsigns
        vim.keymap.set("n", "]h", gs.next_hunk,  { buffer = bufnr, desc = "Next hunk" })
        vim.keymap.set("n", "[h", gs.prev_hunk,  { buffer = bufnr, desc = "Prev hunk" })
        vim.keymap.set("n", "<leader>gb", gs.blame_line, { buffer = bufnr, desc = "Blame line" })
      end,
    },
  },

  { "numToStr/Comment.nvim", opts = {} },

  { "kylechui/nvim-surround", version = "*", opts = {} },

  {
    "nvim-lualine/lualine.nvim",
    dependencies = { "nvim-tree/nvim-web-devicons" },
    opts = { options = { theme = "catppuccin" } },
  },

  {
    "folke/which-key.nvim",
    event = "VeryLazy",
    opts = {},
    keys = {
      { "<leader>", mode = { "n", "v" } },
    },
  },

  { "nvim-tree/nvim-web-devicons" },
  { "nvim-lua/plenary.nvim" },
})

vim.api.nvim_create_autocmd("LspAttach", {
  group = vim.api.nvim_create_augroup("UserLspConfig", {}),
  callback = function(ev)
    local opts = { buffer = ev.buf }
    vim.keymap.set("n", "gd", vim.lsp.buf.definition,        vim.tbl_extend("force", opts, { desc = "Go to definition" }))
    vim.keymap.set("n", "K",  vim.lsp.buf.hover,             vim.tbl_extend("force", opts, { desc = "Hover" }))
    vim.keymap.set("n", "gr", vim.lsp.buf.references,        vim.tbl_extend("force", opts, { desc = "References" }))
    vim.keymap.set("n", "gi", vim.lsp.buf.implementation,    vim.tbl_extend("force", opts, { desc = "Implementation" }))
    vim.keymap.set("n", "<leader>rn", vim.lsp.buf.rename,    vim.tbl_extend("force", opts, { desc = "Rename" }))
    vim.keymap.set("n", "<leader>ca", vim.lsp.buf.code_action, vim.tbl_extend("force", opts, { desc = "Code action" }))
    vim.keymap.set("n", "[d", vim.diagnostic.goto_prev,      vim.tbl_extend("force", opts, { desc = "Prev diagnostic" }))
    vim.keymap.set("n", "]d", vim.diagnostic.goto_next,      vim.tbl_extend("force", opts, { desc = "Next diagnostic" }))
  end,
})

local capabilities = require("blink.cmp").get_lsp_capabilities()
local lspconfig = require("lspconfig")

lspconfig.ts_ls.setup({ capabilities = capabilities })
lspconfig.rust_analyzer.setup({ capabilities = capabilities })
lspconfig.basedpyright.setup({ capabilities = capabilities })
lspconfig.lua_ls.setup({
  capabilities = capabilities,
  settings = {
    Lua = {
      runtime = { version = "LuaJIT" },
      diagnostics = { globals = { "vim" } },
    },
  },
})

vim.cmd.colorscheme("catppuccin-mocha")
NVIMEOF

chown -R gem:gem /home/gem/.config

su - gem -c 'nvim --headless "+Lazy! sync" "+TSInstallSync! typescript tsx rust python json yaml toml bash markdown lua" +qa 2>&1 || true'

chown -R gem:gem /home/gem/.local/share/nvim /home/gem/.cache/nvim 2>/dev/null || true
