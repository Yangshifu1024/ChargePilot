import { Button, Input, Text, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { Notice, PageHeader } from '../../runtime/ui'
import controller from './scan.controller'
import { isH5, developmentLogin } from '../../runtime/platform'

export default function PageView() {
  const { data, event, page } = useController(controller)
  const { busy, error, manualCode } = data
  return <View className='container'>
    <PageHeader title='扫码充电' subtitle={page._reportFault ? '扫描设备或插座二维码，识别后进入设备报修' : '对准桩体或插座上的二维码'} />
    <View className='hero'><View className='hero-body'>
      <Text className='eyebrow'>相机</Text>
      <Text className='hero-title'>扫描二维码</Text>
      {!isH5 && <Button type='primary' onClick={event('scan', {})} loading={busy} disabled={busy}>打开相机扫码</Button>}
      {isH5 && <Text className='hero-desc'>当前浏览器请粘贴二维码内容，或输入设备编号 / 插座码。</Text>}
    </View></View>
    <View className='card'>
      <Text className='heading'>也可以输入设备编号或端口码</Text>
      <Text className='field-label'>设备编号或端口码</Text>
      <Input aria-label='设备编号或端口码' placeholder='请输入设备编号或端口码' maxlength={64} value={manualCode} onInput={event('inputCode', {})} onConfirm={event('manual', {})} />
      <Button type='primary' onClick={event('manual', {})} disabled={busy || !manualCode}>查询</Button>
    </View>
    {developmentLogin && <View className='card development-panel'>
      <Text className='heading'>双模拟器快捷入口</Text>
      <Text className='text-secondary'>先在后台创建对应设备</Text>
      {['5348240514082652', '5348240514082653'].map((id, i) => <Button key={id} onClick={() => { page.setData({ manualCode: id }); page.manual() }} disabled={busy}>模拟器 {i + 1} · {id}</Button>)}
    </View>}
    {!!error && <Notice tone='error'>{error}</Notice>}
  </View>
}
