-- Where a user's winnings are paid: a LUD-16 Lightning Address, lowercase
-- `user@domain`. Set at signup and editable; NULL only for accounts created
-- before payouts required one.
ALTER TABLE user ADD COLUMN lightning_address TEXT;
