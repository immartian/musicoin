// Copyright 2015, 2016 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

import React, { Component, PropTypes } from 'react';
import IconMenu from 'material-ui/IconMenu';
import IconButton from 'material-ui/IconButton/IconButton';
import AccountIcon from 'material-ui/svg-icons/action/account-circle';
import MenuItem from 'material-ui/MenuItem';

import IdentityIcon from '../IdentityIcon';
import renderAddress from '../ui/address';

import styles from './accounts.css';

export default class Accounts extends Component {

  static propTypes = {
    actions: PropTypes.object.isRequired,
    all: PropTypes.object.isRequired,
    selected: PropTypes.object
  }

  render () {
    const { all, selected } = this.props;

    const origin = { horizontal: 'right', vertical: 'top' };

    const accountsButton = (
      <IconButton className={ styles.button }>
        { selected
          ? (<IdentityIcon className={ styles.icon } address={ selected.address } />)
          : (<AccountIcon className={ styles.icon } color='white' />)
        }
      </IconButton>);

    return (
      <IconMenu
        value={ selected ? this.renderAccount(selected) : null }
        onChange={ this.onAccountSelect }
        iconButtonElement={ accountsButton }

        anchorOrigin={ origin }
        targetOrigin={ origin }
      >
        { Object.values(all).map(this.renderAccount) }
      </IconMenu>
    );
  }

  renderAccount = (account) => {
    const { all, selected } = this.props;
    const isSelected = selected && selected.address === account.address;

    return (
      <MenuItem
        key={ account.address } value={ account.address }
        checked={ isSelected } insetChildren={ !isSelected }
      >
        { renderAddress(account.address, all, {}) }
      </MenuItem>
    );
  };

  onAccountSelect = (e, address) => {
    this.props.actions.select(address);
  };
}
