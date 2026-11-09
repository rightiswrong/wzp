## Default Permission

Default permissions for the WarzonePhone Android bridge.

Allows the webview to open the system file picker, drain the queue of files
shared to the app, and ask for the notification permission. Saving to public
Downloads, the internal data dir and the transfer foreground service are only
reachable from Rust (see `WzpAndroidExt`).

#### This default permission set includes the following:

- `allow-pick-files`
- `allow-take-shared-files`
- `allow-request-notification-permission`

## Permission Table

<table>
<tr>
<th>Identifier</th>
<th>Description</th>
</tr>


<tr>
<td>

`wzp-android:allow-pick-files`

</td>
<td>

Enables the pick_files command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`wzp-android:deny-pick-files`

</td>
<td>

Denies the pick_files command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`wzp-android:allow-request-notification-permission`

</td>
<td>

Enables the request_notification_permission command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`wzp-android:deny-request-notification-permission`

</td>
<td>

Denies the request_notification_permission command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`wzp-android:allow-take-shared-files`

</td>
<td>

Enables the take_shared_files command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`wzp-android:deny-take-shared-files`

</td>
<td>

Denies the take_shared_files command without any pre-configured scope.

</td>
</tr>
</table>
