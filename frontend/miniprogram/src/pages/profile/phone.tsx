import { Button, Input, Text, View } from '@tarojs/components'
import { useController, developmentLogin, isH5 } from '../../runtime'
import { Notice, PageHeader } from '../../runtime/ui'
import controller from './phone.controller'

export default function PageView() {
  const { data, event } = useController(controller)
  const { loading, busy, bound, needsLogin, error, notice, phone } = data
  return <View className='container'>
    <PageHeader title='手机号' subtitle={developmentLogin ? '开发环境可填写测试手机号。' : '通过微信授权绑定手机号。'} />
    {needsLogin && <View className='hero'><View className='hero-body'>
      <Text className='hero-title'>登录后可管理手机号绑定</Text>
      <Button type='primary' onClick={event('login', {})}>微信登录</Button>
    </View></View>}
    {!needsLogin && loading && <Notice>正在读取绑定状态…</Notice>}
    {!needsLogin && !loading && <View className='card'>
      <Text className='heading'>手机号绑定</Text>
      <View className={`banner ${bound ? 'banner-ok' : 'banner-warn'}`}>{bound ? '当前账号已绑定手机号' : '当前账号尚未绑定手机号'}</View>
      {!bound && developmentLogin && <>
        <Text className='field-label'>测试手机号</Text>
        <Input placeholder='测试手机号' type='number' maxlength={11} value={phone} onInput={event('inputPhone', {})} />
        <Button type='primary' onClick={event('bindDevelopment', {})} loading={busy} disabled={busy}>绑定测试手机号</Button>
      </>}
      {!bound && !developmentLogin && !isH5 && <Button type='primary' openType='getPhoneNumber' onGetPhoneNumber={event('onGetPhoneNumber', {})} loading={busy} disabled={busy}>授权并绑定手机号</Button>}
      {!bound && !developmentLogin && isH5 && <Text>请在微信小程序内授权绑定手机号。</Text>}
      {bound && <Button onClick={event('unbind', {})} loading={busy} disabled={busy}>解除绑定</Button>}
    </View>}
    {notice && <View className='banner banner-ok'>{notice}</View>}
    {error && <Notice tone='error' retry={needsLogin ? undefined : event('load', {})} busy={busy}>{error}</Notice>}
  </View>
}
